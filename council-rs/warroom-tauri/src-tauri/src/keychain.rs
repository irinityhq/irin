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
//!
//! Keychain selection is independent of Application Support location. Remapping
//! `HOME` for app-data isolation is wrong: it leaves Security.framework without
//! a default login keychain (`errSecNoDefaultKeychain`) and can present a
//! "Keychain Not Found" modal. Use `IRIN_APP_SUPPORT_ROOT` for app-data
//! isolation and keep the operator login keychain.

use std::collections::HashMap;
use std::sync::Mutex;

/// Stable app identity — must match tauri.conf.json `identifier`.
pub const KEYCHAIN_SERVICE: &str = "com.irinity.irin";
/// Legacy app identity from the retired "Council War Room" product name.
/// Read-only: first launch adopts existing operator secrets from this service
/// (see `migrate_legacy_secrets`); the app never writes or deletes items here.
pub const LEGACY_KEYCHAIN_SERVICE: &str = "com.sovereign.council.warroom";
/// Account label for the Council service-role client key.
pub const GW_API_KEY_ACCOUNT: &str = "gateway-client-gw-api-key";
/// Account label for the long-lived auth pepper (never co-mingled with client key).
pub const AUTH_PEPPER_ACCOUNT: &str = "gateway-pack-auth-pepper";

/// Fixed fail-fast token when no usable login keychain is available.
/// Never request interactive Keychain management (Reset To Defaults, create, etc.).
pub const KEYCHAIN_UNAVAILABLE: &str =
    "login keychain unavailable; refusing interactive Keychain management";

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
    use security_framework::os::macos::keychain::SecKeychain;
    use security_framework::passwords::delete_generic_password;
    use security_framework_sys::access_control::kSecAttrAccessibleWhenUnlockedThisDeviceOnly;
    use security_framework_sys::base::{errSecDuplicateItem, errSecItemNotFound, errSecSuccess};
    use security_framework_sys::item::{
        kSecAttrAccount, kSecAttrService, kSecClass, kSecClassGenericPassword, kSecUseKeychain,
        kSecUseAuthenticationUI, kSecUseAuthenticationUISkip, kSecValueData,
    };
    use security_framework_sys::keychain_item::{SecItemAdd, SecItemUpdate};
    use std::ffi::CStr;
    use std::path::{Path, PathBuf};
    use std::ptr;

    use super::KEYCHAIN_UNAVAILABLE;

    // kSecAttrAccessible is the attribute *key*; protection class values live in
    // access_control. Not re-exported by security-framework-sys item module.
    // kSecUseAuthenticationUIFail fails closed without presenting UI.
    #[link(name = "Security", kind = "framework")]
    extern "C" {
        static kSecAttrAccessible: CFStringRef;
        static kSecUseAuthenticationUIFail: CFStringRef;
    }

    /// errSecNoDefaultKeychain — no default keychain in the current session.
    const ERR_SEC_NO_DEFAULT_KEYCHAIN: i32 = -25315;

    fn is_not_found(err: &security_framework::base::Error) -> bool {
        err.code() == errSecItemNotFound
            || err.to_string().contains("could not be found")
            || err.to_string().contains("not found")
            || err.to_string().contains("-25300")
    }

    fn is_no_default_keychain(err: &security_framework::base::Error) -> bool {
        err.code() == ERR_SEC_NO_DEFAULT_KEYCHAIN
            || err.to_string().contains("No keychain is available")
            || err.to_string().contains("no default keychain")
            || err.to_string().contains("-25315")
    }

    /// Resolve the existing login keychain for the current uid only.
    /// Never creates, resets, or rewrites the search list. Never logs the path.
    fn open_existing_login_keychain() -> Result<SecKeychain, String> {
        let path = existing_login_keychain_path().ok_or_else(|| KEYCHAIN_UNAVAILABLE.to_string())?;
        SecKeychain::open(&path).map_err(|_| KEYCHAIN_UNAVAILABLE.to_string())
    }

    fn existing_login_keychain_path() -> Option<PathBuf> {
        let home = pw_dir_for_current_uid()?;
        let db = home.join("Library/Keychains/login.keychain-db");
        if db.is_file() {
            return Some(db);
        }
        let legacy = home.join("Library/Keychains/login.keychain");
        if legacy.is_file() {
            return Some(legacy);
        }
        None
    }

    fn pw_dir_for_current_uid() -> Option<PathBuf> {
        // getpwuid(getuid) — session user's home, not process HOME (smoke may
        // isolate app data without remapping Keychain).
        unsafe {
            let uid = libc::getuid();
            let pw = libc::getpwuid(uid);
            if pw.is_null() {
                return None;
            }
            let dir = (*pw).pw_dir;
            if dir.is_null() {
                return None;
            }
            let c = CStr::from_ptr(dir);
            let s = c.to_str().ok()?;
            if s.is_empty() {
                return None;
            }
            Some(PathBuf::from(s))
        }
    }

    /// Resolve a usable keychain for this call (never logged).
    /// Prefer the session default; if absent, open the existing login keychain
    /// for the current uid only (never create/reset).
    fn resolved_keychain() -> Result<SecKeychain, String> {
        resolve_usable_keychain()
    }

    fn resolve_usable_keychain() -> Result<SecKeychain, String> {
        match SecKeychain::default() {
            Ok(kc) => Ok(kc),
            Err(e) if is_no_default_keychain(&e) => open_existing_login_keychain(),
            Err(_) => {
                // Default failed for another reason — still try existing login only.
                open_existing_login_keychain()
            }
        }
    }

    /// Fail-fast preflight: usable login keychain must already exist.
    /// Never presents interactive Keychain management UI.
    pub fn preflight_keychain_available() -> Result<(), String> {
        resolved_keychain().map(|_| ())
    }

    /// Atomic add-or-update with WhenUnlockedThisDeviceOnly accessibility.
    /// Never delete-then-add (that creates a loss window under concurrent readers
    /// and must not be used as an ACL reclaim path).
    ///
    /// Uses `kSecAttrAccessible` (not SecAccessControl) so ad-hoc/unsigned test
    /// binaries work without a keychain-access-groups entitlement; Developer ID
    /// signed app continuity remains release ceremony.
    ///
    /// Non-interactive: authentication-UI flags fail closed. They do **not**
    /// fix `errSecNoDefaultKeychain` — preflight + explicit `kSecUseKeychain`
    /// against the existing login keychain do.
    pub fn set_password_device_local(
        service: &str,
        account: &str,
        password: &[u8],
    ) -> Result<(), String> {
        let keychain = resolved_keychain()?;
        // 1) Prefer update of existing item (password only — preserves identity).
        match update_password(service, account, password, &keychain) {
            Ok(()) => return Ok(()),
            Err(e) if is_not_found(&e) => {}
            Err(e) => {
                // Fall through to add; some items may reject update if ACL differs.
                // Do **not** delete-and-readd operator items.
                let _ = e;
            }
        }
        // 2) Add with explicit device-local accessibility class.
        match add_password_device_local(service, account, password, &keychain) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == errSecDuplicateItem => {
                // Race: another writer added between update-miss and add.
                update_password(service, account, password, &keychain)
                    .map_err(|e| format!("keychain update after race failed: {e}"))
            }
            Err(e) if is_no_default_keychain(&e) => Err(KEYCHAIN_UNAVAILABLE.to_string()),
            Err(e) => Err(format!("keychain add failed: {e}")),
        }
    }

    fn update_password(
        service: &str,
        account: &str,
        password: &[u8],
        keychain: &SecKeychain,
    ) -> SfResult<()> {
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
            (
                unsafe { CFString::wrap_under_get_rule(kSecUseKeychain) },
                unsafe { CFType::wrap_under_get_rule(keychain.as_CFTypeRef()) },
            ),
            (
                unsafe { CFString::wrap_under_get_rule(kSecUseAuthenticationUI) },
                unsafe {
                    CFString::wrap_under_get_rule(kSecUseAuthenticationUIFail).into_CFType()
                },
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

    fn add_password_device_local(
        service: &str,
        account: &str,
        password: &[u8],
        keychain: &SecKeychain,
    ) -> SfResult<()> {
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
                unsafe { CFString::wrap_under_get_rule(kSecUseKeychain) },
                unsafe { CFType::wrap_under_get_rule(keychain.as_CFTypeRef()) },
            ),
            (
                unsafe { CFString::wrap_under_get_rule(kSecUseAuthenticationUI) },
                // Prefer Skip on add (no auth UI); Fail on query/update above.
                unsafe {
                    CFString::wrap_under_get_rule(kSecUseAuthenticationUISkip).into_CFType()
                },
            ),
            (
                unsafe { CFString::wrap_under_get_rule(kSecValueData) },
                CFData::from_buffer(password).into_CFType(),
            ),
        ];
        let params = CFDictionary::from_CFType_pairs(&pairs);
        let mut ret = ptr::null();
        let status = unsafe { SecItemAdd(params.as_concrete_TypeRef(), &mut ret) };
        if status == errSecSuccess {
            Ok(())
        } else {
            Err(security_framework::base::Error::from_code(status))
        }
    }

    pub fn delete_password_raw(service: &str, account: &str) -> Result<(), String> {
        // Preflight so missing default keychain fails with fixed token.
        let _ = resolved_keychain()?;
        match delete_generic_password(service, account) {
            Ok(()) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) if is_no_default_keychain(&e) => Err(KEYCHAIN_UNAVAILABLE.to_string()),
            Err(e) => Err(format!("keychain delete failed: {e}")),
        }
    }

    pub fn get_password_raw(service: &str, account: &str) -> Result<Option<String>, String> {
        let _ = resolved_keychain()?;
        match security_framework::passwords::get_generic_password(service, account) {
            Ok(bytes) => {
                let s = String::from_utf8(bytes.to_vec())
                    .map_err(|_| "keychain item is not valid UTF-8".to_string())?;
                Ok(Some(s))
            }
            Err(e) => {
                let msg = e.to_string();
                let code = e.code();
                if is_not_found(&e)
                    || msg.contains("could not be found")
                    || msg.contains("not found")
                    || msg.contains("-25300")
                    || code == errSecItemNotFound
                {
                    Ok(None)
                } else if is_no_default_keychain(&e) {
                    Err(KEYCHAIN_UNAVAILABLE.to_string())
                } else {
                    Err(format!("keychain get failed: {e}"))
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn login_keychain_file_exists() -> bool {
        existing_login_keychain_path().is_some()
    }

    /// True when `root` contains a nested login.keychain-db (must never happen
    /// under an isolated app-support root). Path is not logged.
    pub fn app_support_contains_login_keychain(root: &Path) -> bool {
        let a = root.join("Library/Keychains/login.keychain-db");
        let b = root.join("Keychains/login.keychain-db");
        let c = root.join("login.keychain-db");
        a.is_file() || b.is_file() || c.is_file()
    }
}

#[cfg(target_os = "macos")]
impl SecretStore for KeychainSecretStore {
    fn set_password(&self, service: &str, account: &str, password: &str) -> Result<(), String> {
        macos_keychain::set_password_device_local(service, account, password.as_bytes())
    }

    fn get_password(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        macos_keychain::get_password_raw(service, account)
    }

    fn delete_password(&self, service: &str, account: &str) -> Result<(), String> {
        macos_keychain::delete_password_raw(service, account)
    }
}

/// Public preflight for enable path / diagnostics (non-secret fixed error only).
#[cfg(target_os = "macos")]
pub fn preflight_keychain_available() -> Result<(), String> {
    macos_keychain::preflight_keychain_available()
}

#[cfg(not(target_os = "macos"))]
pub fn preflight_keychain_available() -> Result<(), String> {
    Err("Keychain is only available on macOS".to_string())
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

/// One-time, non-destructive adoption of secrets stored by the legacy
/// "Council War Room" build under `LEGACY_KEYCHAIN_SERVICE`.
///
/// For each known account: when the new IRIN service has no item and the
/// legacy service has one, copy the value into the new service. Never
/// deletes the legacy item (a still-installed legacy app keeps working) and
/// never overwrites an existing new item. Per-item errors are tolerated with
/// a secret-free warning: Gateway Pack Enable re-provisions the secret
/// anyway. Called once at app startup.
pub fn migrate_legacy_secrets(store: &impl SecretStore) {
    for account in [GW_API_KEY_ACCOUNT, AUTH_PEPPER_ACCOUNT] {
        let already_present = match store.get_password(KEYCHAIN_SERVICE, account) {
            Ok(value) => value.is_some(),
            Err(e) => {
                eprintln!(
                    "legacy keychain migration: cannot probe {account} under new service ({e}); skipping"
                );
                continue;
            }
        };
        if already_present {
            continue;
        }
        match store.get_password(LEGACY_KEYCHAIN_SERVICE, account) {
            Ok(Some(value)) => {
                if let Err(e) = store.set_password(KEYCHAIN_SERVICE, account, &value) {
                    eprintln!(
                        "legacy keychain migration: cannot write {account} under new service ({e}); skipping"
                    );
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!(
                    "legacy keychain migration: cannot read {account} under legacy service ({e}); skipping"
                );
            }
        }
    }
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

/// Presence-only probe for AUTH_PEPPER account.
pub fn auth_pepper_present(store: &dyn SecretStore) -> Result<bool, String> {
    Ok(load_auth_pepper(store)?.is_some())
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
        assert!(auth_pepper_present(&store).unwrap());
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

    #[test]
    fn unavailable_token_is_fixed_and_non_secret() {
        assert!(KEYCHAIN_UNAVAILABLE.contains("login keychain unavailable"));
        assert!(!KEYCHAIN_UNAVAILABLE.contains('/'));
        assert!(!KEYCHAIN_UNAVAILABLE.to_lowercase().contains("password"));
    }

    #[test]
    fn migrate_legacy_secrets_copies_only_when_new_absent() {
        let store = MemorySecretStore::default();
        let legacy_key = fake_gateway_key('7');
        let legacy_pepper = "ef".repeat(32);
        store
            .set_password(LEGACY_KEYCHAIN_SERVICE, GW_API_KEY_ACCOUNT, &legacy_key)
            .unwrap();
        store
            .set_password(LEGACY_KEYCHAIN_SERVICE, AUTH_PEPPER_ACCOUNT, &legacy_pepper)
            .unwrap();

        migrate_legacy_secrets(&store);
        // Copy happened for both accounts.
        assert_eq!(
            store
                .get_password(KEYCHAIN_SERVICE, GW_API_KEY_ACCOUNT)
                .unwrap(),
            Some(legacy_key.clone())
        );
        assert_eq!(
            store
                .get_password(KEYCHAIN_SERVICE, AUTH_PEPPER_ACCOUNT)
                .unwrap(),
            Some(legacy_pepper.clone())
        );
        // Legacy items are left intact (non-destructive).
        assert_eq!(
            store
                .get_password(LEGACY_KEYCHAIN_SERVICE, GW_API_KEY_ACCOUNT)
                .unwrap(),
            Some(legacy_key.clone())
        );
        assert_eq!(
            store
                .get_password(LEGACY_KEYCHAIN_SERVICE, AUTH_PEPPER_ACCOUNT)
                .unwrap(),
            Some(legacy_pepper)
        );
    }

    #[test]
    fn migrate_legacy_secrets_never_overwrites_existing_new_value() {
        let store = MemorySecretStore::default();
        let new_key = fake_gateway_key('9');
        let legacy_key = fake_gateway_key('7');
        store
            .set_password(KEYCHAIN_SERVICE, GW_API_KEY_ACCOUNT, &new_key)
            .unwrap();
        store
            .set_password(LEGACY_KEYCHAIN_SERVICE, GW_API_KEY_ACCOUNT, &legacy_key)
            .unwrap();

        migrate_legacy_secrets(&store);
        assert_eq!(
            store
                .get_password(KEYCHAIN_SERVICE, GW_API_KEY_ACCOUNT)
                .unwrap(),
            Some(new_key)
        );
        // Legacy item is left intact even when no copy was needed.
        assert_eq!(
            store
                .get_password(LEGACY_KEYCHAIN_SERVICE, GW_API_KEY_ACCOUNT)
                .unwrap(),
            Some(legacy_key)
        );
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
    fn live_keychain_preflight_ok_on_operator_session() {
        if std::env::var("IRIN_KEYCHAIN_LIVE_TEST").ok().as_deref() != Some("1") {
            eprintln!("skip live keychain preflight (set IRIN_KEYCHAIN_LIVE_TEST=1)");
            return;
        }
        preflight_keychain_available().expect("login keychain must be available");
    }

    #[test]
    fn live_keychain_create_read_update_delete_unique_service() {
        if std::env::var("IRIN_KEYCHAIN_LIVE_TEST").ok().as_deref() != Some("1") {
            eprintln!("skip live keychain test (set IRIN_KEYCHAIN_LIVE_TEST=1)");
            return;
        }
        let service = format!(
            "com.irinity.irin.test.{}",
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
            "com.irinity.irin.test.conc.{}",
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
            "com.irinity.irin.test.missing.{}",
            std::process::id()
        );
        let store = KeychainSecretStore;
        assert!(store
            .get_password(&service, "no-such-account")
            .unwrap()
            .is_none());
    }

    #[test]
    fn live_keychain_pepper_and_key_presence_only_unique_service() {
        if std::env::var("IRIN_KEYCHAIN_LIVE_TEST").ok().as_deref() != Some("1") {
            return;
        }
        let service = format!(
            "com.irinity.irin.test.presence.{}",
            std::process::id()
        );
        let store = KeychainSecretStore;
        let key = format!("gw_{}", "a".repeat(32));
        let pepper = "cd".repeat(32);
        store
            .set_password(&service, GW_API_KEY_ACCOUNT, &key)
            .unwrap();
        store
            .set_password(&service, AUTH_PEPPER_ACCOUNT, &pepper)
            .unwrap();
        assert!(store
            .get_password(&service, GW_API_KEY_ACCOUNT)
            .unwrap()
            .is_some());
        assert!(store
            .get_password(&service, AUTH_PEPPER_ACCOUNT)
            .unwrap()
            .is_some());
        store
            .delete_password(&service, GW_API_KEY_ACCOUNT)
            .unwrap();
        store
            .delete_password(&service, AUTH_PEPPER_ACCOUNT)
            .unwrap();
    }
}
