//! Device-local generic-password storage for Gateway Pack secrets.
//!
//! - Raw `GW_API_KEY` (Council→Gateway client key)
//! - Long-lived `AUTH_PEPPER` (separate account)
//!
//! Never write these to private.json, localStorage, Compose yaml, durable app
//! env files that the renderer can read, command arguments, receipts, or logs.
//!
//! Production uses macOS Security.framework with atomic add/update and
//! `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`. Unit tests use in-memory.

use std::collections::HashMap;
use std::sync::Mutex;

/// Stable app identity — must match tauri.conf.json `identifier`.
pub const KEYCHAIN_SERVICE: &str = "com.sovereign.council.warroom";
/// Account label for the Council service-role client key.
pub const GW_API_KEY_ACCOUNT: &str = "gateway-client-gw-api-key";
/// Account label for the long-lived auth pepper (never co-mingled with client key).
pub const AUTH_PEPPER_ACCOUNT: &str = "gateway-pack-auth-pepper";

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
mod macos_keychain {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::data::CFData;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;
    use core_foundation_sys::string::CFStringRef;
    use security_framework::base::Result as SfResult;
    use security_framework::passwords::delete_generic_password;
    use security_framework_sys::access_control::kSecAttrAccessibleWhenUnlockedThisDeviceOnly;
    use security_framework_sys::base::{errSecDuplicateItem, errSecItemNotFound, errSecSuccess};
    use security_framework_sys::item::{
        kSecAttrAccount, kSecAttrService, kSecClass, kSecClassGenericPassword, kSecValueData,
    };
    use security_framework_sys::keychain_item::{SecItemAdd, SecItemUpdate};

    // kSecAttrAccessible is the attribute *key*; protection class values live in
    // access_control. Not re-exported by security-framework-sys item module.
    #[link(name = "Security", kind = "framework")]
    extern "C" {
        static kSecAttrAccessible: CFStringRef;
    }

    fn is_not_found(err: &security_framework::base::Error) -> bool {
        err.code() == errSecItemNotFound
            || err.to_string().contains("could not be found")
            || err.to_string().contains("not found")
            || err.to_string().contains("-25300")
    }

    /// Atomic add-or-update with WhenUnlockedThisDeviceOnly accessibility.
    /// Never delete-then-add (that creates a loss window under concurrent readers).
    ///
    /// Uses `kSecAttrAccessible` (not SecAccessControl) so ad-hoc/unsigned test
    /// binaries work without a keychain-access-groups entitlement; Developer ID
    /// signed app continuity remains release ceremony.
    ///
    /// Non-interactive: never present Keychain UI (packaged smoke / headless
    /// automation must fail closed rather than hang on a modal).
    pub fn set_password_device_local(
        service: &str,
        account: &str,
        password: &[u8],
    ) -> Result<(), String> {
        // 1) Prefer update of existing item (password only — preserves identity).
        match update_password(service, account, password) {
            Ok(()) => return Ok(()),
            Err(e) if is_not_found(&e) => {}
            Err(e) => {
                // Fall through to add; some items may reject update if ACL differs
                // (e.g. prior Developer ID vs ad-hoc identity).
                let _ = e;
            }
        }
        // 2) Add with explicit device-local accessibility class.
        match add_password_device_local(service, account, password) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == errSecDuplicateItem => {
                // Race or ACL-blocked update: delete then re-add so ad-hoc / signed
                // identity can reclaim the item. Brief loss window only when the
                // preferred update path already failed.
                let _ = delete_password_raw(service, account);
                add_password_device_local(service, account, password)
                    .map_err(|e2| format!("keychain re-add after reclaim failed: {e2}"))
            }
            Err(e) => {
                // Last resort: reclaim and re-add (covers ACL-denied update+add).
                let _ = delete_password_raw(service, account);
                match add_password_device_local(service, account, password) {
                    Ok(()) => Ok(()),
                    Err(e2) => Err(format!(
                        "keychain add failed: {e}; reclaim re-add failed: {e2}"
                    )),
                }
            }
        }
    }

    fn update_password(service: &str, account: &str, password: &[u8]) -> SfResult<()> {
        let query = CFDictionary::from_CFType_pairs(&[
            (
                unsafe { CFString::wrap_under_get_rule(kSecClass) },
                unsafe { CFString::wrap_under_get_rule(kSecClassGenericPassword).into_CFType() },
            ),
            (
                unsafe { CFString::wrap_under_get_rule(kSecAttrService) },
                CFString::from(service).into_CFType(),
            ),
            (
                unsafe { CFString::wrap_under_get_rule(kSecAttrAccount) },
                CFString::from(account).into_CFType(),
            ),
        ]);
        let update = CFDictionary::from_CFType_pairs(&[(
            unsafe { CFString::wrap_under_get_rule(kSecValueData) },
            CFData::from_buffer(password).into_CFType(),
        )]);
        let status = unsafe {
            SecItemUpdate(query.as_concrete_TypeRef(), update.as_concrete_TypeRef())
        };
        if status == errSecSuccess {
            Ok(())
        } else {
            Err(security_framework::base::Error::from_code(status))
        }
    }

    fn add_password_device_local(service: &str, account: &str, password: &[u8]) -> SfResult<()> {
        let pairs: Vec<(CFString, CFType)> = vec![
            (
                unsafe { CFString::wrap_under_get_rule(kSecClass) },
                unsafe { CFString::wrap_under_get_rule(kSecClassGenericPassword).into_CFType() },
            ),
            (
                unsafe { CFString::wrap_under_get_rule(kSecAttrService) },
                CFString::from(service).into_CFType(),
            ),
            (
                unsafe { CFString::wrap_under_get_rule(kSecAttrAccount) },
                CFString::from(account).into_CFType(),
            ),
            (
                unsafe { CFString::wrap_under_get_rule(kSecAttrAccessible) },
                unsafe {
                    CFString::wrap_under_get_rule(kSecAttrAccessibleWhenUnlockedThisDeviceOnly)
                        .into_CFType()
                },
            ),
            (
                unsafe { CFString::wrap_under_get_rule(kSecValueData) },
                CFData::from_buffer(password).into_CFType(),
            ),
        ];
        let params = CFDictionary::from_CFType_pairs(&pairs);
        let mut ret = std::ptr::null();
        let status = unsafe { SecItemAdd(params.as_concrete_TypeRef(), &mut ret) };
        if status == errSecSuccess {
            Ok(())
        } else {
            Err(security_framework::base::Error::from_code(status))
        }
    }

    pub fn delete_password_raw(service: &str, account: &str) -> Result<(), String> {
        match delete_generic_password(service, account) {
            Ok(()) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(format!("keychain delete failed: {e}")),
        }
    }
}

#[cfg(target_os = "macos")]
impl SecretStore for KeychainSecretStore {
    fn set_password(&self, service: &str, account: &str, password: &str) -> Result<(), String> {
        macos_keychain::set_password_device_local(service, account, password.as_bytes())
    }

    fn get_password(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        match security_framework::passwords::get_generic_password(service, account) {
            Ok(bytes) => {
                let s = String::from_utf8(bytes.to_vec())
                    .map_err(|_| "keychain item is not valid UTF-8".to_string())?;
                Ok(Some(s))
            }
            Err(e) => {
                let msg = e.to_string();
                let code = e.code();
                if msg.contains("could not be found")
                    || msg.contains("not found")
                    || msg.contains("-25300")
                    || code == security_framework_sys::base::errSecItemNotFound
                {
                    Ok(None)
                } else if msg.contains("User interaction is not allowed")
                    || msg.contains("interaction is not allowed")
                    || msg.contains("-25308")
                    || msg.contains("auth")
                    || msg.contains("ACL")
                    || msg.contains("denied")
                    || code == -25293 // errSecAuthFailed
                    || code == -25308 // errSecInteractionNotAllowed
                {
                    // Unreadable under this process identity (prior ACL / lock).
                    // Treat as absent so set_password can reclaim the item.
                    Ok(None)
                } else {
                    Err(format!("keychain get failed: {e}"))
                }
            }
        }
    }

    fn delete_password(&self, service: &str, account: &str) -> Result<(), String> {
        macos_keychain::delete_password_raw(service, account)
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

/// Store / load the long-lived AUTH_PEPPER (hex, >= 32 chars). Separate Keychain account.
pub fn store_auth_pepper(store: &dyn SecretStore, pepper: &str) -> Result<(), String> {
    let trimmed = pepper.trim();
    if !is_valid_auth_pepper(trimmed) {
        return Err("refusing to store invalid AUTH_PEPPER shape".to_string());
    }
    store.set_password(KEYCHAIN_SERVICE, AUTH_PEPPER_ACCOUNT, trimmed)
}

pub fn load_auth_pepper(store: &dyn SecretStore) -> Result<Option<String>, String> {
    store.get_password(KEYCHAIN_SERVICE, AUTH_PEPPER_ACCOUNT)
}

pub fn delete_auth_pepper(store: &dyn SecretStore) -> Result<(), String> {
    store.delete_password(KEYCHAIN_SERVICE, AUTH_PEPPER_ACCOUNT)
}

pub fn delete_all_gateway_pack_secrets(store: &dyn SecretStore) -> Result<(), String> {
    delete_gw_api_key(store)?;
    delete_auth_pepper(store)?;
    Ok(())
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

/// AUTH_PEPPER: 32+ hex chars (we generate 64 hex = 32 bytes).
pub fn is_valid_auth_pepper(pepper: &str) -> bool {
    let b = pepper.as_bytes();
    b.len() >= 32 && b.len() <= 128 && b.iter().all(|c| c.is_ascii_hexdigit())
}

/// Presence-only probe: returns whether the Keychain item exists without
/// returning the secret value (for receipts).
pub fn gw_api_key_present(store: &dyn SecretStore) -> Result<bool, String> {
    Ok(load_gw_api_key(store)?.is_some())
}

/// Redact a secret for logs: never include the raw value.
pub fn redact_secret(value: &str) -> String {
    if value.is_empty() {
        return "<empty>".to_string();
    }
    if is_valid_gw_raw_key(value) {
        return "gw_***".to_string();
    }
    if is_valid_auth_pepper(value) {
        return "<pepper:***>".to_string();
    }
    if value.len() <= 4 {
        return "***".to_string();
    }
    format!("{}***", &value[..2.min(value.len())])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_gateway_key(nibble: char) -> String {
        format!("gw_{}", nibble.to_string().repeat(32))
    }

    #[test]
    fn memory_store_round_trip_and_delete() {
        let store = MemorySecretStore::default();
        let key = fake_gateway_key('a');
        store_gw_api_key(&store, &key).unwrap();
        let got = load_gw_api_key(&store).unwrap().unwrap();
        assert_eq!(got, key);
        delete_gw_api_key(&store).unwrap();
        assert!(load_gw_api_key(&store).unwrap().is_none());
    }

    #[test]
    fn memory_store_update_is_atomic_no_delete_gap() {
        let store = MemorySecretStore::default();
        let k1 = fake_gateway_key('1');
        let k2 = fake_gateway_key('2');
        store_gw_api_key(&store, &k1).unwrap();
        // Concurrent-style update: set without delete.
        store_gw_api_key(&store, &k2).unwrap();
        assert_eq!(load_gw_api_key(&store).unwrap().unwrap(), k2);
        assert!(gw_api_key_present(&store).unwrap());
    }

    #[test]
    fn pepper_separate_account() {
        let store = MemorySecretStore::default();
        let pepper = "ab".repeat(32);
        store_auth_pepper(&store, &pepper).unwrap();
        assert_eq!(load_auth_pepper(&store).unwrap().unwrap(), pepper);
        // Client key account remains empty.
        assert!(load_gw_api_key(&store).unwrap().is_none());
        delete_all_gateway_pack_secrets(&store).unwrap();
        assert!(load_auth_pepper(&store).unwrap().is_none());
    }

    #[test]
    fn rejects_invalid_key_shape() {
        let store = MemorySecretStore::default();
        assert!(store_gw_api_key(&store, "not-a-key").is_err());
        assert!(store_gw_api_key(&store, "gw_short").is_err());
        assert!(store_auth_pepper(&store, "short").is_err());
        assert!(store_auth_pepper(&store, "not-hex!!").is_err());
    }

    #[test]
    fn redaction_never_echoes_raw() {
        let key = fake_gateway_key('b');
        let r = redact_secret(&key);
        assert!(!r.contains(&"b".repeat(8)));
        assert_eq!(r, "gw_***");
    }

    #[test]
    fn valid_key_predicate() {
        assert!(is_valid_gw_raw_key(&fake_gateway_key('c')));
        assert!(!is_valid_gw_raw_key(&format!("gw_{}", "d".repeat(31))));
        assert!(!is_valid_gw_raw_key("sk-foo"));
    }
}

/// Live Keychain integration test — only runs when explicitly enabled so CI/unit
/// runs never touch the operator Keychain by default.
#[cfg(all(test, target_os = "macos"))]
mod keychain_live_tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn live_keychain_create_read_update_delete_unique_service() {
        if std::env::var("IRIN_KEYCHAIN_LIVE_TEST").ok().as_deref() != Some("1") {
            eprintln!("skip live keychain test (set IRIN_KEYCHAIN_LIVE_TEST=1)");
            return;
        }
        let service = format!(
            "com.sovereign.council.warroom.test.{}",
            std::process::id()
        );
        let account = "gateway-client-gw-api-key-test";
        let key1 = format!("gw_{}", "e".repeat(32));
        let key2 = format!("gw_{}", "f".repeat(32));
        let store = KeychainSecretStore;
        // create
        store.set_password(&service, account, &key1).unwrap();
        let got = store.get_password(&service, account).unwrap().unwrap();
        assert_eq!(got, key1);
        // update without delete gap
        store.set_password(&service, account, &key2).unwrap();
        let got2 = store.get_password(&service, account).unwrap().unwrap();
        assert_eq!(got2, key2);
        // delete
        store.delete_password(&service, account).unwrap();
        assert!(store.get_password(&service, account).unwrap().is_none());
        // Never print key1/key2/got.
    }

    #[test]
    fn live_keychain_concurrent_updates_last_writer_wins() {
        if std::env::var("IRIN_KEYCHAIN_LIVE_TEST").ok().as_deref() != Some("1") {
            eprintln!("skip live keychain concurrency test");
            return;
        }
        let service = format!(
            "com.sovereign.council.warroom.test.conc.{}",
            std::process::id()
        );
        let account = "gateway-client-gw-api-key-test";
        let store = Arc::new(KeychainSecretStore);
        store
            .set_password(&service, account, &format!("gw_{}", "0".repeat(32)))
            .unwrap();
        let mut handles = Vec::new();
        for n in 0..4u8 {
            let store = Arc::clone(&store);
            let service = service.clone();
            handles.push(thread::spawn(move || {
                let key = format!("gw_{}", format!("{n:x}").repeat(32));
                store.set_password(&service, account, &key)
            }));
        }
        for h in handles {
            h.join().unwrap().unwrap();
        }
        let final_val = store.get_password(&service, account).unwrap().unwrap();
        assert!(is_valid_gw_raw_key(&final_val));
        store.delete_password(&service, account).unwrap();
    }

    #[test]
    fn live_keychain_get_missing_is_none_not_error() {
        if std::env::var("IRIN_KEYCHAIN_LIVE_TEST").ok().as_deref() != Some("1") {
            return;
        }
        let service = format!(
            "com.sovereign.council.warroom.test.missing.{}",
            std::process::id()
        );
        let store = KeychainSecretStore;
        assert!(store
            .get_password(&service, "no-such-account")
            .unwrap()
            .is_none());
    }
}
