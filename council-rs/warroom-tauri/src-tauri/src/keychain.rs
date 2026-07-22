//! Device-local generic-password storage for the Council→Gateway client key.
//!
//! Raw `GW_API_KEY` lives only here (or in-memory during a single provision call).
//! Never write it to private.json, localStorage, Compose yaml, env files that the
//! renderer can read, command arguments, receipts, or logs.
//!
//! Production uses macOS Security.framework via the `security-framework` crate
//! (not the `security` CLI). Unit tests use an in-memory store.

use std::collections::HashMap;
use std::sync::Mutex;

/// Stable app identity — must match tauri.conf.json `identifier`.
pub const KEYCHAIN_SERVICE: &str = "com.sovereign.council.warroom";
/// Account label for the Council service-role client key.
pub const GW_API_KEY_ACCOUNT: &str = "gateway-client-gw-api-key";

/// Abstraction so tests never touch the real Keychain.
pub trait SecretStore: Send + Sync {
    fn set_password(&self, service: &str, account: &str, password: &str) -> Result<(), String>;
    fn get_password(&self, service: &str, account: &str) -> Result<Option<String>, String>;
    fn delete_password(&self, service: &str, account: &str) -> Result<(), String>;
}

/// In-memory store for tests. Values are never printed.
#[derive(Default)]
pub struct MemorySecretStore {
    inner: Mutex<HashMap<(String, String), String>>,
}

impl SecretStore for MemorySecretStore {
    fn set_password(&self, service: &str, account: &str, password: &str) -> Result<(), String> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| "memory secret store lock poisoned".to_string())?;
        g.insert((service.to_string(), account.to_string()), password.to_string());
        Ok(())
    }

    fn get_password(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        let g = self
            .inner
            .lock()
            .map_err(|_| "memory secret store lock poisoned".to_string())?;
        Ok(g.get(&(service.to_string(), account.to_string())).cloned())
    }

    fn delete_password(&self, service: &str, account: &str) -> Result<(), String> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| "memory secret store lock poisoned".to_string())?;
        g.remove(&(service.to_string(), account.to_string()));
        Ok(())
    }
}

/// macOS Security.framework-backed store.
#[derive(Default)]
pub struct KeychainSecretStore;

#[cfg(target_os = "macos")]
impl SecretStore for KeychainSecretStore {
    fn set_password(&self, service: &str, account: &str, password: &str) -> Result<(), String> {
        // Delete existing item first so updates replace rather than fail.
        let _ = self.delete_password(service, account);
        security_framework::passwords::set_generic_password(service, account, password.as_bytes())
            .map_err(|e| format!("keychain set failed: {e}"))
    }

    fn get_password(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        match security_framework::passwords::get_generic_password(service, account) {
            Ok(bytes) => {
                let s = String::from_utf8(bytes.to_vec())
                    .map_err(|_| "keychain item is not valid UTF-8".to_string())?;
                Ok(Some(s))
            }
            Err(e) => {
                // Item-not-found is a normal miss, not an error.
                let msg = e.to_string();
                if msg.contains("could not be found")
                    || msg.contains("not found")
                    || msg.contains("-25300")
                {
                    Ok(None)
                } else {
                    Err(format!("keychain get failed: {e}"))
                }
            }
        }
    }

    fn delete_password(&self, service: &str, account: &str) -> Result<(), String> {
        match security_framework::passwords::delete_generic_password(service, account) {
            Ok(()) => Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("could not be found")
                    || msg.contains("not found")
                    || msg.contains("-25300")
                {
                    Ok(())
                } else {
                    Err(format!("keychain delete failed: {e}"))
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl SecretStore for KeychainSecretStore {
    fn set_password(&self, _service: &str, _account: &str, _password: &str) -> Result<(), String> {
        Err("Keychain is only available on macOS".to_string())
    }
    fn get_password(&self, _service: &str, _account: &str) -> Result<Option<String>, String> {
        Err("Keychain is only available on macOS".to_string())
    }
    fn delete_password(&self, _service: &str, _account: &str) -> Result<(), String> {
        Err("Keychain is only available on macOS".to_string())
    }
}

/// Store the Council client GW_API_KEY under the stable app identity.
pub fn store_gw_api_key(store: &dyn SecretStore, raw_key: &str) -> Result<(), String> {
    let trimmed = raw_key.trim();
    if !is_valid_gw_raw_key(trimmed) {
        return Err("refusing to store invalid GW_API_KEY shape".to_string());
    }
    store.set_password(KEYCHAIN_SERVICE, GW_API_KEY_ACCOUNT, trimmed)
}

pub fn load_gw_api_key(store: &dyn SecretStore) -> Result<Option<String>, String> {
    store.get_password(KEYCHAIN_SERVICE, GW_API_KEY_ACCOUNT)
}

pub fn delete_gw_api_key(store: &dyn SecretStore) -> Result<(), String> {
    store.delete_password(KEYCHAIN_SERVICE, GW_API_KEY_ACCOUNT)
}

/// Gateway client keys are `gw_` + 32 hex chars (see sidecar auth.rs).
pub fn is_valid_gw_raw_key(key: &str) -> bool {
    let b = key.as_bytes();
    if b.len() != 3 + 32 {
        return false;
    }
    if &b[0..3] != b"gw_" {
        return false;
    }
    b[3..].iter().all(|c| c.is_ascii_hexdigit())
}

/// Redact a secret for logs: never include the raw value.
pub fn redact_secret(value: &str) -> String {
    if value.is_empty() {
        return "<empty>".to_string();
    }
    if is_valid_gw_raw_key(value) {
        return "gw_***".to_string();
    }
    if value.len() <= 4 {
        return "***".to_string();
    }
    format!("{}***", &value[..2.min(value.len())])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trip_and_delete() {
        let store = MemorySecretStore::default();
        let key = "gw_0123456789abcdef0123456789abcdef";
        store_gw_api_key(&store, key).unwrap();
        let got = load_gw_api_key(&store).unwrap().unwrap();
        assert_eq!(got, key);
        delete_gw_api_key(&store).unwrap();
        assert!(load_gw_api_key(&store).unwrap().is_none());
    }

    #[test]
    fn rejects_invalid_key_shape() {
        let store = MemorySecretStore::default();
        assert!(store_gw_api_key(&store, "not-a-key").is_err());
        assert!(store_gw_api_key(&store, "gw_short").is_err());
    }

    #[test]
    fn redaction_never_echoes_raw() {
        let key = "gw_0123456789abcdef0123456789abcdef";
        let r = redact_secret(key);
        assert!(!r.contains("0123456789"));
        assert_eq!(r, "gw_***");
    }

    #[test]
    fn valid_key_predicate() {
        assert!(is_valid_gw_raw_key("gw_0123456789abcdef0123456789abcdef"));
        assert!(!is_valid_gw_raw_key("gw_0123456789abcdef0123456789abcde")); // 31
        assert!(!is_valid_gw_raw_key("sk-foo"));
    }
}

/// Live Keychain integration test — only runs when explicitly enabled so CI/unit
/// runs never touch the operator Keychain by default.
#[cfg(all(test, target_os = "macos"))]
mod keychain_live_tests {
    use super::*;

    #[test]
    fn live_keychain_round_trip_unique_service() {
        if std::env::var("IRIN_KEYCHAIN_LIVE_TEST").ok().as_deref() != Some("1") {
            eprintln!("skip live keychain test (set IRIN_KEYCHAIN_LIVE_TEST=1)");
            return;
        }
        let service = format!(
            "com.sovereign.council.warroom.test.{}",
            std::process::id()
        );
        let account = "gateway-client-gw-api-key-test";
        let key = "gw_fedcba9876543210fedcba9876543210";
        let store = KeychainSecretStore;
        // Use raw API with unique service so we never collide with production.
        store.set_password(&service, account, key).unwrap();
        let got = store.get_password(&service, account).unwrap().unwrap();
        assert_eq!(got, key);
        // Never print got/key.
        store.delete_password(&service, account).unwrap();
        assert!(store.get_password(&service, account).unwrap().is_none());
    }
}
