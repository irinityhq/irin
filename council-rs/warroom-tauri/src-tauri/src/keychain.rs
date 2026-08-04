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

#[cfg(test)]
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
/// Account label for the Watch/Outbox admin read token (`WATCH_ADMIN_TOKEN`).
/// Held only in the Keychain and the per-spawn Compose/Council process env;
/// never written to the public env file, never returned to the renderer,
/// never logged.
pub const WATCH_ADMIN_TOKEN_ACCOUNT: &str = "gateway-pack-watch-admin-token";
/// Account label for the Touch ID bridge's arm-principal bearer token — the
/// `GW_ARM_PRINCIPALS` custody-domain-1 credential for this installed app.
/// Held only in the Keychain and the per-spawn Compose process env; never
/// written to the public env file, never returned to the renderer, never
/// logged.
pub const ARM_PRINCIPAL_TOKEN_ACCOUNT: &str = "gateway-pack-arm-principal-token";
/// Account label for the Claude host-adapter shared secret (`CLAUDE_PROXY_TOKEN`).
/// Held only in the Keychain and the per-spawn Compose process env; never
/// written to the public env file, never returned to the renderer, never logged.
pub const CLAUDE_PROXY_TOKEN_ACCOUNT: &str = "gateway-pack-claude-proxy-token";
/// Account label for the Codex host-adapter shared secret (`CODEX_PROXY_TOKEN`).
pub const CODEX_PROXY_TOKEN_ACCOUNT: &str = "gateway-pack-codex-proxy-token";

/// Fixed principal name for the single-operator desktop bridge. The token is
/// the secret; the name is a stable, non-secret audit label that appears in
/// the sidecar's hash-chained `arm_audit` rows.
pub const ARM_PRINCIPAL_NAME: &str = "irin-desktop";

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
#[cfg(test)]
#[derive(Default)]
pub struct MemorySecretStore {
    inner: Mutex<HashMap<(String, String), String>>,
}

#[cfg(test)]
impl SecretStore for MemorySecretStore {
    fn set_password(&self, service: &str, account: &str, password: &str) -> Result<(), String> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| "memory secret store lock poisoned".to_string())?;
        g.insert(
            (service.to_string(), account.to_string()),
            password.to_string(),
        );
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
    use core_foundation::boolean::CFBoolean;
    use core_foundation::data::CFData;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;
    use core_foundation_sys::base::{CFGetTypeID, CFRelease, CFTypeRef};
    use core_foundation_sys::data::CFDataRef;
    use core_foundation_sys::string::CFStringRef;
    use security_framework::base::Result as SfResult;
    use security_framework::os::macos::keychain::SecKeychain;
    use security_framework_sys::access_control::kSecAttrAccessibleWhenUnlockedThisDeviceOnly;
    use security_framework_sys::base::{
        errSecDuplicateItem, errSecItemNotFound, errSecParam, errSecSuccess,
    };
    use security_framework_sys::item::{
        kSecAttrAccount, kSecAttrService, kSecClass, kSecClassGenericPassword, kSecReturnData,
        kSecUseAuthenticationUI, kSecUseAuthenticationUISkip, kSecUseKeychain, kSecValueData,
    };
    use security_framework_sys::keychain_item::{
        SecItemAdd, SecItemCopyMatching, SecItemDelete, SecItemUpdate,
    };
    use std::ffi::CStr;
    use std::path::PathBuf;
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
        let path =
            existing_login_keychain_path().ok_or_else(|| KEYCHAIN_UNAVAILABLE.to_string())?;
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
    #[cfg(test)]
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
                unsafe { CFString::wrap_under_get_rule(kSecUseAuthenticationUIFail).into_CFType() },
            ),
        ]);
        let update = CFDictionary::from_CFType_pairs(&[(
            unsafe { CFString::wrap_under_get_rule(kSecValueData) },
            CFData::from_buffer(password).into_CFType(),
        )]);
        let status =
            unsafe { SecItemUpdate(query.as_concrete_TypeRef(), update.as_concrete_TypeRef()) };
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
                unsafe { CFString::wrap_under_get_rule(kSecUseAuthenticationUISkip).into_CFType() },
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

    /// Build a generic-password query pinned to the same resolved login
    /// keychain used by add/update. This avoids accidentally reading from or
    /// deleting an identically named item elsewhere in the process search list.
    fn generic_password_query(
        service: &str,
        account: &str,
        keychain: &SecKeychain,
        return_data: bool,
    ) -> CFDictionary<CFString, CFType> {
        let mut pairs: Vec<(CFString, CFType)> = vec![
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
                unsafe { CFString::wrap_under_get_rule(kSecUseAuthenticationUIFail).into_CFType() },
            ),
        ];
        if return_data {
            pairs.push((
                unsafe { CFString::wrap_under_get_rule(kSecReturnData) },
                CFBoolean::from(true).into_CFType(),
            ));
        }
        CFDictionary::from_CFType_pairs(&pairs)
    }

    fn get_generic_password_from_keychain(
        service: &str,
        account: &str,
        keychain: &SecKeychain,
    ) -> SfResult<Vec<u8>> {
        let query = generic_password_query(service, account, keychain, true);
        let mut ret: CFTypeRef = ptr::null();
        let status = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &mut ret) };
        if status != errSecSuccess {
            return Err(security_framework::base::Error::from_code(status));
        }
        if !ret.is_null() && unsafe { CFGetTypeID(ret) } == CFData::type_id() {
            let data = unsafe { CFData::wrap_under_create_rule(ret as CFDataRef) };
            return Ok(data.bytes().to_vec());
        }
        if !ret.is_null() {
            unsafe { CFRelease(ret) };
        }
        Err(security_framework::base::Error::from_code(errSecParam))
    }

    fn delete_generic_password_from_keychain(
        service: &str,
        account: &str,
        keychain: &SecKeychain,
    ) -> SfResult<()> {
        let query = generic_password_query(service, account, keychain, false);
        let status = unsafe { SecItemDelete(query.as_concrete_TypeRef()) };
        if status == errSecSuccess {
            Ok(())
        } else {
            Err(security_framework::base::Error::from_code(status))
        }
    }

    pub fn delete_password_raw(service: &str, account: &str) -> Result<(), String> {
        let keychain = resolved_keychain()?;
        match delete_generic_password_from_keychain(service, account, &keychain) {
            Ok(()) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) if is_no_default_keychain(&e) => Err(KEYCHAIN_UNAVAILABLE.to_string()),
            Err(e) => Err(format!("keychain delete failed: {e}")),
        }
    }

    pub fn get_password_raw(service: &str, account: &str) -> Result<Option<String>, String> {
        let keychain = resolved_keychain()?;
        match get_generic_password_from_keychain(service, account, &keychain) {
            Ok(bytes) => {
                let s = String::from_utf8(bytes)
                    .map_err(|_| "keychain item is not valid UTF-8".to_string())?;
                Ok(Some(s))
            }
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) if is_no_default_keychain(&e) => Err(KEYCHAIN_UNAVAILABLE.to_string()),
            Err(e) => Err(format!("keychain get failed: {e}")),
        }
    }

    #[allow(dead_code)]
    pub fn login_keychain_file_exists() -> bool {
        existing_login_keychain_path().is_some()
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

/// Preflight probe (non-secret fixed error only). Only the gated live Keychain
/// integration test calls this today; kept test-only until the enable path
/// needs it again.
#[cfg(test)]
#[cfg(target_os = "macos")]
pub fn preflight_keychain_available() -> Result<(), String> {
    macos_keychain::preflight_keychain_available()
}

#[cfg(test)]
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

/// Watch/Outbox admin read token: same hex shape as AUTH_PEPPER (32+ hex
/// chars; we generate 64 hex = 32 bytes). Separate Keychain account.
pub fn is_valid_watch_admin_token(token: &str) -> bool {
    is_valid_auth_pepper(token)
}

pub fn store_watch_admin_token(store: &dyn SecretStore, token: &str) -> Result<(), String> {
    let trimmed = token.trim();
    if !is_valid_watch_admin_token(trimmed) {
        return Err("refusing to store invalid WATCH_ADMIN_TOKEN shape".to_string());
    }
    store.set_password(KEYCHAIN_SERVICE, WATCH_ADMIN_TOKEN_ACCOUNT, trimmed)
}

pub fn load_watch_admin_token(store: &dyn SecretStore) -> Result<Option<String>, String> {
    store.get_password(KEYCHAIN_SERVICE, WATCH_ADMIN_TOKEN_ACCOUNT)
}

pub fn delete_watch_admin_token(store: &dyn SecretStore) -> Result<(), String> {
    store.delete_password(KEYCHAIN_SERVICE, WATCH_ADMIN_TOKEN_ACCOUNT)
}

/// Touch ID bridge: the arm-principal bearer token. Shape is the same
/// `tok_` + 32 hex the sidecar's principal registry accepts as an opaque
/// value; the strict shape check keeps a malformed/injected value (CR/LF, `:`
/// or `,` — the `GW_ARM_PRINCIPALS` separators) out of the registry string.
pub fn store_arm_principal_token(store: &dyn SecretStore, token: &str) -> Result<(), String> {
    let trimmed = token.trim();
    if !is_valid_arm_principal_token(trimmed) {
        return Err("refusing to store invalid arm-principal token shape".to_string());
    }
    store.set_password(KEYCHAIN_SERVICE, ARM_PRINCIPAL_TOKEN_ACCOUNT, trimmed)?;
    seed_arm_principal_observation(true);
    Ok(())
}

pub fn load_arm_principal_token(store: &dyn SecretStore) -> Result<Option<String>, String> {
    // Fail closed on a stored value that no longer satisfies the shape rule
    // (tampered item, older format): treat it as absent so the pack boots
    // arm-incapable rather than shipping a malformed registry string.
    Ok(store
        .get_password(KEYCHAIN_SERVICE, ARM_PRINCIPAL_TOKEN_ACCOUNT)?
        .filter(|v| is_valid_arm_principal_token(v)))
}

pub fn delete_arm_principal_token(store: &dyn SecretStore) -> Result<(), String> {
    store.delete_password(KEYCHAIN_SERVICE, ARM_PRINCIPAL_TOKEN_ACCOUNT)?;
    seed_arm_principal_observation(false);
    Ok(())
}

/// Lifecycle-scoped, presence-only observation for the background status loop.
/// The raw bearer is never cached. Authority callers pass a live token and
/// bypass this observation entirely.
static ARM_PRINCIPAL_OBSERVATION: Mutex<Option<bool>> = Mutex::new(None);

pub enum ArmPrincipalProbeMode<'a> {
    BackgroundCached,
    AuthorityLive(Option<&'a str>),
}

pub fn seed_arm_principal_observation(present: bool) {
    if let Ok(mut observation) = ARM_PRINCIPAL_OBSERVATION.lock() {
        *observation = Some(present);
    }
}

#[cfg(test)]
pub fn invalidate_arm_principal_observation() {
    if let Ok(mut observation) = ARM_PRINCIPAL_OBSERVATION.lock() {
        *observation = None;
    }
}

/// Returns presence plus a live token only when this call actually loaded or
/// was handed one. A background cache hit deliberately returns no token, so it
/// cannot accidentally become an authorization path.
pub fn resolve_arm_principal(
    store: &dyn SecretStore,
    mode: ArmPrincipalProbeMode<'_>,
) -> (bool, Option<String>) {
    match mode {
        ArmPrincipalProbeMode::AuthorityLive(token) => (token.is_some(), token.map(str::to_string)),
        ArmPrincipalProbeMode::BackgroundCached => {
            if let Ok(observation) = ARM_PRINCIPAL_OBSERVATION.lock() {
                if let Some(present) = *observation {
                    return (present, None);
                }
            }
            let token = load_arm_principal_token(store).ok().flatten();
            seed_arm_principal_observation(token.is_some());
            (token.is_some(), token)
        }
    }
}

/// `tok_` + 32 hex. Rejects every `GW_ARM_PRINCIPALS` separator and every
/// injection byte by construction (hex only).
pub fn is_valid_arm_principal_token(token: &str) -> bool {
    let b = token.as_bytes();
    if b.len() != 4 + 32 {
        return false;
    }
    if &b[0..4] != b"tok_" {
        return false;
    }
    b[4..].iter().all(|c| c.is_ascii_hexdigit())
}

/// Host-adapter shared secret: 64 lowercase hex chars (32 random bytes).
/// Same shape the app-owned adapter path mints for `CLAUDE_PROXY_TOKEN` /
/// `CODEX_PROXY_TOKEN`.
pub fn is_valid_proxy_token(token: &str) -> bool {
    let b = token.as_bytes();
    b.len() == 64 && b.iter().all(|c| c.is_ascii_hexdigit())
}

pub fn store_claude_proxy_token(store: &dyn SecretStore, token: &str) -> Result<(), String> {
    let trimmed = token.trim();
    if !is_valid_proxy_token(trimmed) {
        return Err("refusing to store invalid CLAUDE_PROXY_TOKEN shape".to_string());
    }
    store.set_password(KEYCHAIN_SERVICE, CLAUDE_PROXY_TOKEN_ACCOUNT, trimmed)
}

pub fn load_claude_proxy_token(store: &dyn SecretStore) -> Result<Option<String>, String> {
    Ok(store
        .get_password(KEYCHAIN_SERVICE, CLAUDE_PROXY_TOKEN_ACCOUNT)?
        .filter(|v| is_valid_proxy_token(v)))
}

pub fn delete_claude_proxy_token(store: &dyn SecretStore) -> Result<(), String> {
    store.delete_password(KEYCHAIN_SERVICE, CLAUDE_PROXY_TOKEN_ACCOUNT)
}

pub fn store_codex_proxy_token(store: &dyn SecretStore, token: &str) -> Result<(), String> {
    let trimmed = token.trim();
    if !is_valid_proxy_token(trimmed) {
        return Err("refusing to store invalid CODEX_PROXY_TOKEN shape".to_string());
    }
    store.set_password(KEYCHAIN_SERVICE, CODEX_PROXY_TOKEN_ACCOUNT, trimmed)
}

pub fn load_codex_proxy_token(store: &dyn SecretStore) -> Result<Option<String>, String> {
    Ok(store
        .get_password(KEYCHAIN_SERVICE, CODEX_PROXY_TOKEN_ACCOUNT)?
        .filter(|v| is_valid_proxy_token(v)))
}

pub fn delete_codex_proxy_token(store: &dyn SecretStore) -> Result<(), String> {
    store.delete_password(KEYCHAIN_SERVICE, CODEX_PROXY_TOKEN_ACCOUNT)
}

pub fn delete_all_gateway_pack_secrets(store: &dyn SecretStore) -> Result<(), String> {
    // Attempt every account even if one fails so a single ACL error cannot
    // leave the other secret behind while the caller thinks uninstall finished.
    let mut errors: Vec<String> = Vec::new();
    if let Err(e) = delete_gw_api_key(store) {
        errors.push(format!("GW_API_KEY: {e}"));
    }
    if let Err(e) = delete_auth_pepper(store) {
        errors.push(format!("AUTH_PEPPER: {e}"));
    }
    // Uninstall removes the watch-admin read token too: a pack that no longer
    // exists must not leave a live admin credential behind.
    if let Err(e) = delete_watch_admin_token(store) {
        errors.push(format!("WATCH_ADMIN_TOKEN: {e}"));
    }
    // Uninstall removes the arm-principal token too: a pack that no longer
    // exists must not leave a live custody-domain-1 credential behind.
    if let Err(e) = delete_arm_principal_token(store) {
        errors.push(format!("ARM_PRINCIPAL_TOKEN: {e}"));
    }
    // Host-adapter tokens are pack-scoped secrets: uninstall clears them so a
    // later re-enable mints fresh values.
    if let Err(e) = delete_claude_proxy_token(store) {
        errors.push(format!("CLAUDE_PROXY_TOKEN: {e}"));
    }
    if let Err(e) = delete_codex_proxy_token(store) {
        errors.push(format!("CODEX_PROXY_TOKEN: {e}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Keychain delete incomplete ({})",
            errors.join("; ")
        ))
    }
}

/// Values observed while checking the two legacy-migration accounts. Returning
/// them lets cold launch reuse the same Keychain reads instead of immediately
/// prompting for those accounts again.
#[derive(Debug, Default)]
pub struct MigratedLegacySecrets {
    pub gw_api_key: Option<String>,
    pub auth_pepper: Option<String>,
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
pub fn migrate_legacy_secrets_with_values(store: &dyn SecretStore) -> MigratedLegacySecrets {
    let mut migrated = MigratedLegacySecrets::default();
    for account in [GW_API_KEY_ACCOUNT, AUTH_PEPPER_ACCOUNT] {
        let value = match store.get_password(KEYCHAIN_SERVICE, account) {
            Ok(value) => value,
            Err(e) => {
                eprintln!(
                    "legacy keychain migration: cannot probe {account} under new service ({e}); skipping"
                );
                continue;
            }
        };
        let value = if value.is_some() {
            value
        } else {
            match store.get_password(LEGACY_KEYCHAIN_SERVICE, account) {
                Ok(Some(value)) => {
                    if let Err(e) = store.set_password(KEYCHAIN_SERVICE, account, &value) {
                        eprintln!(
                        "legacy keychain migration: cannot write {account} under new service ({e}); skipping"
                    );
                        None
                    } else {
                        Some(value)
                    }
                }
                Ok(None) => None,
                Err(e) => {
                    eprintln!(
                    "legacy keychain migration: cannot read {account} under legacy service ({e}); skipping"
                );
                    None
                }
            }
        };
        match account {
            GW_API_KEY_ACCOUNT => migrated.gw_api_key = value,
            AUTH_PEPPER_ACCOUNT => migrated.auth_pepper = value,
            _ => unreachable!(),
        }
    }
    migrated
}

#[cfg(test)]
pub fn migrate_legacy_secrets(store: &dyn SecretStore) {
    let _ = migrate_legacy_secrets_with_values(store);
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

/// Presence-only test probe: returns whether the in-memory Keychain item exists
/// without returning the secret value.
#[cfg(test)]
pub fn gw_api_key_present(store: &dyn SecretStore) -> Result<bool, String> {
    Ok(load_gw_api_key(store)?.is_some())
}

/// Presence-only probe for AUTH_PEPPER account.
#[cfg(test)]
pub fn auth_pepper_present(store: &dyn SecretStore) -> Result<bool, String> {
    Ok(load_auth_pepper(store)?.is_some())
}

/// Redact a secret for logs: never include the raw value.
#[cfg(test)]
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
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn fake_gateway_key(nibble: char) -> String {
        format!("gw_{}", nibble.to_string().repeat(32))
    }

    struct CountingArmStore {
        inner: MemorySecretStore,
        gets: AtomicUsize,
    }

    impl CountingArmStore {
        fn new() -> Self {
            Self {
                inner: MemorySecretStore::default(),
                gets: AtomicUsize::new(0),
            }
        }
    }

    impl SecretStore for CountingArmStore {
        fn set_password(&self, service: &str, account: &str, password: &str) -> Result<(), String> {
            self.inner.set_password(service, account, password)
        }

        fn get_password(&self, service: &str, account: &str) -> Result<Option<String>, String> {
            if service == KEYCHAIN_SERVICE && account == ARM_PRINCIPAL_TOKEN_ACCOUNT {
                self.gets.fetch_add(1, AtomicOrdering::SeqCst);
            }
            self.inner.get_password(service, account)
        }

        fn delete_password(&self, service: &str, account: &str) -> Result<(), String> {
            self.inner.delete_password(service, account)
        }
    }

    #[test]
    fn arm_principal_background_cache_hit_avoids_store_read() {
        let _lock = crate::private_config::test_env_lock();
        let store = CountingArmStore::new();
        store
            .inner
            .set_password(
                KEYCHAIN_SERVICE,
                ARM_PRINCIPAL_TOKEN_ACCOUNT,
                &format!("tok_{:032x}", 1u128),
            )
            .unwrap();
        invalidate_arm_principal_observation();

        let (present, token) =
            resolve_arm_principal(&store, ArmPrincipalProbeMode::BackgroundCached);
        assert!(present && token.is_some());
        assert_eq!(store.gets.load(AtomicOrdering::SeqCst), 1);
        let (present, token) =
            resolve_arm_principal(&store, ArmPrincipalProbeMode::BackgroundCached);
        assert!(present && token.is_none());
        assert_eq!(store.gets.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn arm_principal_store_delete_update_cached_presence() {
        let _lock = crate::private_config::test_env_lock();
        let store = CountingArmStore::new();
        invalidate_arm_principal_observation();
        store_arm_principal_token(&store, &format!("tok_{:032x}", 2u128)).unwrap();
        let (present, _) = resolve_arm_principal(&store, ArmPrincipalProbeMode::BackgroundCached);
        assert!(present);
        assert_eq!(store.gets.load(AtomicOrdering::SeqCst), 0);

        delete_arm_principal_token(&store).unwrap();
        let (present, _) = resolve_arm_principal(&store, ArmPrincipalProbeMode::BackgroundCached);
        assert!(!present);
        assert_eq!(store.gets.load(AtomicOrdering::SeqCst), 0);
    }

    #[test]
    fn arm_principal_authority_none_bypasses_cached_presence() {
        let _lock = crate::private_config::test_env_lock();
        let store = CountingArmStore::new();
        seed_arm_principal_observation(true);
        let (present, token) =
            resolve_arm_principal(&store, ArmPrincipalProbeMode::AuthorityLive(None));
        assert!(!present && token.is_none());
        assert_eq!(store.gets.load(AtomicOrdering::SeqCst), 0);
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
    fn watch_admin_token_round_trip_and_delete_all() {
        let store = MemorySecretStore::default();
        let token = "ab".repeat(32);
        store_watch_admin_token(&store, &token).unwrap();
        assert_eq!(load_watch_admin_token(&store).unwrap().unwrap(), token);
        // Separate account: pepper and client key stay empty.
        assert!(load_auth_pepper(&store).unwrap().is_none());
        assert!(load_gw_api_key(&store).unwrap().is_none());
        assert!(store_watch_admin_token(&store, "short").is_err());
        assert!(store_watch_admin_token(&store, "not-hex!!").is_err());
        delete_all_gateway_pack_secrets(&store).unwrap();
        assert!(load_watch_admin_token(&store).unwrap().is_none());
    }

    /// Store that fails delete for one account so we prove both deletes run.
    struct FailGwDeleteStore {
        inner: MemorySecretStore,
    }

    impl SecretStore for FailGwDeleteStore {
        fn set_password(&self, service: &str, account: &str, password: &str) -> Result<(), String> {
            self.inner.set_password(service, account, password)
        }
        fn get_password(&self, service: &str, account: &str) -> Result<Option<String>, String> {
            self.inner.get_password(service, account)
        }
        fn delete_password(&self, service: &str, account: &str) -> Result<(), String> {
            if account == GW_API_KEY_ACCOUNT {
                return Err("simulated ACL deny on GW_API_KEY".into());
            }
            self.inner.delete_password(service, account)
        }
    }

    #[test]
    fn delete_all_attempts_both_accounts_when_one_fails() {
        let store = FailGwDeleteStore {
            inner: MemorySecretStore::default(),
        };
        let key = fake_gateway_key('e');
        let pepper = "cd".repeat(32);
        store_gw_api_key(&store, &key).unwrap();
        store_auth_pepper(&store, &pepper).unwrap();

        let err = delete_all_gateway_pack_secrets(&store).unwrap_err();
        assert!(
            err.contains("GW_API_KEY") && err.contains("Keychain delete incomplete"),
            "err={err}"
        );
        // Pepper delete still ran despite GW failure.
        assert!(
            load_auth_pepper(&store).unwrap().is_none(),
            "pepper must still be deleted when GW delete fails"
        );
        // GW key remains (delete refused).
        assert_eq!(
            load_gw_api_key(&store).unwrap().as_deref(),
            Some(key.as_str())
        );
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

    #[test]
    fn proxy_token_shape_and_roundtrip() {
        let store = MemorySecretStore::default();
        assert!(!is_valid_proxy_token("short"));
        assert!(!is_valid_proxy_token(&"GG".repeat(32)));
        let tok = "ab".repeat(32);
        let codex = "cd".repeat(32);
        assert!(is_valid_proxy_token(&tok));
        store_claude_proxy_token(&store, &tok).unwrap();
        store_codex_proxy_token(&store, &codex).unwrap();
        assert_eq!(
            load_claude_proxy_token(&store).unwrap().as_deref(),
            Some(tok.as_str())
        );
        assert_eq!(
            load_codex_proxy_token(&store).unwrap().as_deref(),
            Some(codex.as_str())
        );
        delete_claude_proxy_token(&store).unwrap();
        assert!(load_claude_proxy_token(&store).unwrap().is_none());
        assert!(store_claude_proxy_token(&store, "not-hex").is_err());
    }

    #[test]
    fn delete_all_clears_proxy_tokens() {
        let store = MemorySecretStore::default();
        store_claude_proxy_token(&store, &"ab".repeat(32)).unwrap();
        store_codex_proxy_token(&store, &"cd".repeat(32)).unwrap();
        store_auth_pepper(&store, &"ef".repeat(32)).unwrap();
        delete_all_gateway_pack_secrets(&store).unwrap();
        assert!(load_claude_proxy_token(&store).unwrap().is_none());
        assert!(load_codex_proxy_token(&store).unwrap().is_none());
        assert!(load_auth_pepper(&store).unwrap().is_none());
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
        let service = format!("com.irinity.irin.test.{}", std::process::id());
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
        let service = format!("com.irinity.irin.test.conc.{}", std::process::id());
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
        let service = format!("com.irinity.irin.test.missing.{}", std::process::id());
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
        let service = format!("com.irinity.irin.test.presence.{}", std::process::id());
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
        store.delete_password(&service, GW_API_KEY_ACCOUNT).unwrap();
        store
            .delete_password(&service, AUTH_PEPPER_ACCOUNT)
            .unwrap();
    }
}
