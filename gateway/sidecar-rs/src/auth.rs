// ==========================================================================
// auth.rs — Gateway authentication and rate-limiting service.
//
// Implements:
// - Virtual API keys stored in a flat JSON file.
// - Keys are hashed using SHA-256 + an environment pepper.
// - Token-Bucket rate limiting (Global, per-IP, per-Key).
// ==========================================================================

use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthKey {
    #[serde(default)]
    pub key_id: String, // Stable, non-secret identifier (e.g. "k_abc12345"). Safe for logs/metrics.
    pub key_hash: String, // Hex string of SHA-256(pepper + raw_key)
    pub budget_key: String,
    pub tier: String,
    pub rate_limit_rpm: u32,
    #[serde(default)]
    pub rate_limit_burst: u32,
    /// Immutable role tag (spec §5.6). When `Some("council")`, this key is
    /// allowed to restore stashed `X-Council-*` headers in the gateway. The
    /// router additionally requires `key_id == COUNCIL_GATEWAY_KEY_ID` for
    /// defense-in-depth — distinct from `budget_key`, which is an
    /// admin-mutable billing label. `provision_key` rejects mismatched
    /// re-provisioning on the same `key_id` so the role cannot be flipped
    /// silently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_role: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub keys: Vec<AuthKey>,
    #[serde(default = "default_global_rpm")]
    pub global_rpm: u32,
    #[serde(default)]
    pub global_burst: u32,
    #[serde(default = "default_ip_rpm")]
    pub ip_rpm: u32,
    #[serde(default)]
    pub ip_burst: u32,
}

fn default_global_rpm() -> u32 {
    1000
}
fn default_ip_rpm() -> u32 {
    120
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            keys: vec![],
            global_rpm: default_global_rpm(),
            global_burst: 0,
            ip_rpm: default_ip_rpm(),
            ip_burst: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthDecision {
    pub allowed: bool,
    pub reason: String,
    pub key_id: String,
    pub budget_key: String,
    pub tier: String,
    pub rate_limit_limit: u32,
    pub rate_limit_remaining: u32,
    pub rate_limit_reset: u64, // seconds until reset
    /// Surfaced to the gateway router so it can gate the X-Council-* header
    /// restore on `service_role == "council"` AND `key_id == COUNCIL_GATEWAY_KEY_ID`
    /// (spec §5.6). Omitted from the JSON envelope when None to keep payloads
    /// untouched for non-council keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_role: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProvisionResponse {
    pub key_id: String,
    pub key_hash: String,
    pub raw_key: String,
    pub budget_key: String,
    pub tier: String,
}

// ---------------------------------------------------------------------------
// IP Policy (CIDR-based allow / deny gate)
// ---------------------------------------------------------------------------

/// CIDR list pre-parsed at deserialization/construction time. Every string
/// entry goes through `parse_cidr_or_host` exactly once — bare IPs normalize
/// to host routes (/32 or /128), garbage warns and drops at load — so no code
/// path can string-parse CIDRs per-request, and no list can silently carry an
/// entry that never matches. A newtype makes the invariant compiler-enforced
/// instead of conventional.
#[derive(Debug, Clone, Default)]
pub struct CidrList(Vec<IpNet>);

impl CidrList {
    fn from_strs<'a, I: IntoIterator<Item = &'a str>>(entries: I) -> Self {
        Self(entries.into_iter().filter_map(parse_cidr_or_host).collect())
    }

    fn extend_from_strs<'a, I: IntoIterator<Item = &'a str>>(&mut self, entries: I) {
        self.0
            .extend(entries.into_iter().filter_map(parse_cidr_or_host));
    }

    fn contains(&self, ip: &IpAddr) -> bool {
        self.0.iter().any(|net| net.contains(ip))
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}

impl<'de> serde::Deserialize<'de> for CidrList {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Vec::<String>::deserialize(d)?;
        Ok(Self::from_strs(raw.iter().map(String::as_str)))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct IpPolicy {
    #[serde(default = "default_internal_cidrs")]
    pub trusted_internal_cidrs: CidrList,
    /// CIDRs exempt from the GLOBAL and PER-IP rate-limit buckets (per-key
    /// limits always apply). Default EMPTY — exemption is explicit opt-in,
    /// never inherited from `trusted_internal_cidrs`: those cover all of
    /// RFC1918 + loopback, which on a localhost-bound deployment would
    /// silently exempt every client. Also extendable via the
    /// `RATE_LIMIT_EXEMPT_CIDRS` env var (comma-separated), so the smoke
    /// harness can opt in without shipping a policy file.
    #[serde(default)]
    pub rate_limit_exempt_cidrs: CidrList,
    #[serde(default)]
    pub deny_cidrs: CidrList,
    #[serde(default)]
    pub allow_cidrs: CidrList,
    #[serde(default = "default_ip_mode")]
    pub mode: String,
}

fn default_internal_cidrs() -> CidrList {
    CidrList::from_strs([
        "127.0.0.0/8",
        "10.0.0.0/8",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "::1/128",
    ])
}

fn default_ip_mode() -> String {
    "allow_internal_deny_explicit".to_string()
}

impl Default for IpPolicy {
    fn default() -> Self {
        Self {
            trusted_internal_cidrs: default_internal_cidrs(),
            rate_limit_exempt_cidrs: CidrList::default(),
            deny_cidrs: CidrList::default(),
            allow_cidrs: CidrList::default(),
            mode: default_ip_mode(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IpCheckResult {
    pub allowed: bool,
    pub reason: String,
    pub is_internal: bool,
}

fn load_ip_policy() -> IpPolicy {
    let mut policy = std::env::var("IP_POLICY_PATH")
        .ok()
        .and_then(|p| std::fs::read_to_string(&p).ok())
        .and_then(|s| serde_json::from_str::<IpPolicy>(&s).ok())
        .unwrap_or_default();
    if let Ok(v) = std::env::var("RATE_LIMIT_EXEMPT_CIDRS") {
        policy
            .rate_limit_exempt_cidrs
            .extend_from_strs(v.split(',').map(str::trim).filter(|s| !s.is_empty()));
    }
    policy
}

/// Parse a CIDR string, normalizing a bare IP (no `/prefix`) to a host
/// route (/32 or /128). Warns and returns None on entries that parse as
/// neither — load-time noise beats a silently ignored policy entry.
fn parse_cidr_or_host(entry: &str) -> Option<IpNet> {
    if let Ok(net) = entry.parse::<IpNet>() {
        return Some(net);
    }
    if let Ok(ip) = entry.parse::<IpAddr>() {
        return Some(IpNet::from(ip));
    }
    warn!(
        entry,
        "IP-policy CIDR entry is not a valid CIDR or IP — dropped"
    );
    None
}

// ---------------------------------------------------------------------------
// Token Bucket
// ---------------------------------------------------------------------------

pub(crate) struct TokenBucket {
    tokens: f64,
    last_update: Instant,
    capacity: f64,
    fill_rate: f64, // tokens per second
}

impl TokenBucket {
    pub(crate) fn new(burst: u32, rpm: u32) -> Self {
        let actual_burst = if burst > 0 { burst } else { rpm };
        Self {
            tokens: actual_burst as f64,
            last_update: Instant::now(),
            capacity: actual_burst as f64,
            fill_rate: (rpm as f64) / 60.0,
        }
    }

    pub(crate) fn consume(&mut self, amount: f64) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_update).as_secs_f64();

        self.tokens += elapsed * self.fill_rate;
        if self.tokens > self.capacity {
            self.tokens = self.capacity;
        }
        self.last_update = now;

        if self.tokens >= amount {
            self.tokens -= amount;
            true
        } else {
            false
        }
    }

    fn remaining(&self) -> u32 {
        self.tokens.floor() as u32
    }

    fn reset_time(&self) -> u64 {
        if self.tokens >= self.capacity {
            0
        } else {
            ((self.capacity - self.tokens) / self.fill_rate).ceil() as u64
        }
    }
}

// ---------------------------------------------------------------------------
// Auth Service
// ---------------------------------------------------------------------------

pub struct AuthService {
    config_path: Option<PathBuf>,
    pepper: String,
    /// Serializes read-modify-write of the auth config file. Without this,
    /// concurrent provision/revoke calls can race and lose writes (the config
    /// is reloaded fresh from disk inside each call). Held across the whole
    /// load-mutate-write-reload cycle in `provision_key` / `revoke_key`.
    config_mutex: tokio::sync::Mutex<()>,
    keys: RwLock<HashMap<String, AuthKey>>,
    global_bucket: RwLock<TokenBucket>,
    ip_buckets: RwLock<HashMap<String, TokenBucket>>,
    key_buckets: RwLock<HashMap<String, TokenBucket>>,
    global_rpm: u32,
    ip_rpm: u32,
    /// When true, an empty keys map causes `check()` to fail closed (deny by default)
    /// rather than silently allow anything to pass through. Controlled by
    /// `GATEWAY_AUTH_FAIL_CLOSED` (default: "true").
    fail_closed: bool,
    /// CIDR-based IP allow/deny policy. Loaded from `IP_POLICY_PATH` at
    /// startup and on SIGHUP. Defaults to allow_internal_deny_explicit
    /// with the standard private/loopback ranges trusted. Uses std RwLock
    /// (not tokio's) so `check_ip` can stay synchronous on the hot path.
    ip_policy: std::sync::RwLock<IpPolicy>,
}

impl AuthService {
    pub fn new(config_path: Option<PathBuf>) -> Self {
        let fail_closed = std::env::var("GATEWAY_AUTH_FAIL_CLOSED")
            .unwrap_or_else(|_| "true".to_string())
            .eq_ignore_ascii_case("true");

        // Pepper is mandatory in fail-closed (production) mode. Falling back
        // to a hardcoded default would silently make the SHA-256(pepper||key)
        // hashes brutable from the auth_keys.json file alone.
        let pepper = match std::env::var("AUTH_PEPPER") {
            Ok(p) if !p.is_empty() => p,
            _ => {
                if fail_closed {
                    panic!(
                        "FATAL: AUTH_PEPPER not set and GATEWAY_AUTH_FAIL_CLOSED=true. \
                         Set AUTH_PEPPER to a strong random secret in production. \
                         Set GATEWAY_AUTH_FAIL_CLOSED=false for development without pepper."
                    );
                }
                warn!("AUTH_PEPPER not set — using insecure default. NOT SAFE FOR PRODUCTION.");
                "gateway_default_pepper_dev_only".to_string()
            }
        };

        let ip_policy = load_ip_policy();
        info!(
            mode = %ip_policy.mode,
            internal_cidrs = ip_policy.trusted_internal_cidrs.len(),
            deny_cidrs = ip_policy.deny_cidrs.len(),
            allow_cidrs = ip_policy.allow_cidrs.len(),
            "IP policy loaded"
        );

        let mut service = Self {
            config_path,
            pepper,
            config_mutex: tokio::sync::Mutex::new(()),
            keys: RwLock::new(HashMap::new()),
            global_bucket: RwLock::new(TokenBucket::new(0, default_global_rpm())),
            ip_buckets: RwLock::new(HashMap::new()),
            key_buckets: RwLock::new(HashMap::new()),
            global_rpm: default_global_rpm(),
            ip_rpm: default_ip_rpm(),
            fail_closed,
            ip_policy: std::sync::RwLock::new(ip_policy),
        };

        service.reload_sync();
        service
    }

    pub async fn reload(&self) {
        let (config, err) = self.load_config();
        if let Some(err) = err {
            warn!("Failed to reload auth config: {}", err);
            return;
        }

        let mut keys_write = self.keys.write().await;
        keys_write.clear();
        for key in &config.keys {
            keys_write.insert(key.key_hash.clone(), key.clone());
        }

        // We do not reset the buckets on reload to prevent rate limit bypasses.
        // However, we update capacities.
        let mut gb = self.global_bucket.write().await;
        let global_burst = if config.global_burst > 0 {
            config.global_burst
        } else {
            config.global_rpm
        };
        gb.capacity = global_burst as f64;
        gb.fill_rate = (config.global_rpm as f64) / 60.0;

        info!(
            "Auth config reloaded: {} keys, global_rpm={}, ip_rpm={}",
            config.keys.len(),
            config.global_rpm,
            config.ip_rpm
        );

        // Reload IP policy alongside auth config — both are SIGHUP-driven.
        let new_ip_policy = load_ip_policy();
        if let Ok(mut guard) = self.ip_policy.write() {
            info!(
                mode = %new_ip_policy.mode,
                internal_cidrs = new_ip_policy.trusted_internal_cidrs.len(),
                deny_cidrs = new_ip_policy.deny_cidrs.len(),
                allow_cidrs = new_ip_policy.allow_cidrs.len(),
                "IP policy reloaded"
            );
            *guard = new_ip_policy;
        } else {
            warn!("IP policy reload skipped: poisoned RwLock");
        }
    }

    fn reload_sync(&mut self) {
        let (config, err) = self.load_config();
        if let Some(err) = err {
            warn!("Failed to load auth config: {}", err);
            return;
        }

        let mut keys_map = HashMap::new();
        for key in &config.keys {
            keys_map.insert(key.key_hash.clone(), key.clone());
        }

        self.keys = RwLock::new(keys_map);
        self.global_bucket = RwLock::new(TokenBucket::new(config.global_burst, config.global_rpm));
        self.global_rpm = config.global_rpm;
        self.ip_rpm = config.ip_rpm;

        info!("Auth config loaded: {} keys", config.keys.len());
    }

    fn load_config(&self) -> (AuthConfig, Option<String>) {
        if let Some(path) = &self.config_path {
            if path.exists() {
                match std::fs::read_to_string(path) {
                    Ok(content) => match serde_json::from_str::<AuthConfig>(&content) {
                        Ok(cfg) => return (cfg, None),
                        Err(e) => {
                            return (AuthConfig::default(), Some(format!("Invalid JSON: {}", e)))
                        }
                    },
                    Err(e) => return (AuthConfig::default(), Some(format!("IO Error: {}", e))),
                }
            }
        }
        (AuthConfig::default(), None)
    }

    pub fn hash_key(&self, raw_key: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.pepper.as_bytes());
        hasher.update(raw_key.as_bytes());
        hex::encode(hasher.finalize())
    }

    pub async fn check(&self, raw_key: &str, ip: &str) -> AuthDecision {
        // 0. Fail-closed check — if no keys are configured and fail_closed is on,
        // deny by default rather than silently letting traffic through.
        // This protects against a misconfigured deploy (missing/empty auth_keys.json)
        // accidentally turning the gateway into an open relay.
        if self.fail_closed {
            let keys = self.keys.read().await;
            if keys.is_empty() {
                drop(keys);
                return AuthDecision {
                    allowed: false,
                    reason: "Auth system unavailable — no keys configured".to_string(),
                    key_id: "".to_string(),
                    budget_key: "".to_string(),
                    tier: "".to_string(),
                    rate_limit_limit: 0,
                    rate_limit_remaining: 0,
                    rate_limit_reset: 0,
                    service_role: None,
                };
            }
        }

        let rate_limit_exempt = self.is_rate_limit_exempt(ip);

        // 1. Global Rate Limit Check — skipped only for explicitly configured
        // rate_limit_exempt_cidrs (empty by default). Deliberately NOT keyed on
        // is_internal: trusted_internal_cidrs cover all of RFC1918 + loopback,
        // which on a localhost-bound deployment is every client. Per-key limits
        // (step 4) always apply regardless of exemption.
        if !rate_limit_exempt {
            let mut gb = self.global_bucket.write().await;
            if !gb.consume(1.0) {
                return AuthDecision {
                    allowed: false,
                    reason: "Global rate limit exceeded".to_string(),
                    key_id: "".to_string(),
                    budget_key: "".to_string(),
                    tier: "".to_string(),
                    rate_limit_limit: self.global_rpm,
                    rate_limit_remaining: 0,
                    rate_limit_reset: gb.reset_time(),
                    service_role: None,
                };
            }
        }

        // 2. IP Rate Limit Check — same explicit exemption (see above).
        if !rate_limit_exempt {
            let mut ip_buckets = self.ip_buckets.write().await;
            let ip_rpm = self.ip_rpm;
            let bucket = ip_buckets
                .entry(ip.to_string())
                .or_insert_with(|| TokenBucket::new(0, ip_rpm));
            if !bucket.consume(1.0) {
                return AuthDecision {
                    allowed: false,
                    reason: "IP rate limit exceeded".to_string(),
                    key_id: "".to_string(),
                    budget_key: "".to_string(),
                    tier: "".to_string(),
                    rate_limit_limit: self.ip_rpm,
                    rate_limit_remaining: 0,
                    rate_limit_reset: bucket.reset_time(),
                    service_role: None,
                };
            }
        }

        // 3. Key Authentication
        let key_hash = self.hash_key(raw_key);
        let auth_key = {
            let keys = self.keys.read().await;
            keys.get(&key_hash).cloned()
        };

        if let Some(ak) = auth_key {
            // 4. Per-Key Rate Limit Check
            let mut key_buckets = self.key_buckets.write().await;
            let bucket = key_buckets
                .entry(key_hash.clone())
                .or_insert_with(|| TokenBucket::new(ak.rate_limit_burst, ak.rate_limit_rpm));

            if bucket.consume(1.0) {
                AuthDecision {
                    allowed: true,
                    reason: "".to_string(),
                    key_id: ak.key_id.clone(),
                    budget_key: ak.budget_key,
                    tier: ak.tier,
                    rate_limit_limit: ak.rate_limit_rpm,
                    rate_limit_remaining: bucket.remaining(),
                    rate_limit_reset: bucket.reset_time(),
                    service_role: ak.service_role,
                }
            } else {
                AuthDecision {
                    allowed: false,
                    reason: "Key rate limit exceeded".to_string(),
                    key_id: ak.key_id.clone(),
                    budget_key: ak.budget_key,
                    tier: ak.tier,
                    rate_limit_limit: ak.rate_limit_rpm,
                    rate_limit_remaining: 0,
                    rate_limit_reset: bucket.reset_time(),
                    service_role: ak.service_role,
                }
            }
        } else {
            AuthDecision {
                allowed: false,
                reason: "Invalid API key".to_string(),
                key_id: "".to_string(),
                budget_key: "".to_string(),
                tier: "".to_string(),
                rate_limit_limit: 0,
                rate_limit_remaining: 0,
                rate_limit_reset: 0,
                service_role: None,
            }
        }
    }

    /// True when `ip_str` falls inside an explicitly configured
    /// `rate_limit_exempt_cidrs` entry. Exempts the global and per-IP
    /// buckets only — per-key limits still apply. The list is pre-parsed
    /// `IpNet`s (CidrList) — no string parsing on the hot path. Unparseable
    /// IPs and an empty list (the default) are never exempt.
    fn is_rate_limit_exempt(&self, ip_str: &str) -> bool {
        let policy = match self.ip_policy.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if policy.rate_limit_exempt_cidrs.is_empty() {
            return false;
        }
        let ip: IpAddr = match ip_str.parse() {
            Ok(addr) => addr,
            Err(_) => return false,
        };
        policy.rate_limit_exempt_cidrs.contains(&ip)
    }

    /// CIDR-based IP gate. Synchronous and cheap — meant to be called on the
    /// auth hot path right after `check()`. Returns `is_internal` so callers
    /// can apply downstream policy (e.g. trust elevated headers only from
    /// internal sources). Modes:
    ///   - `disabled`                       : pass-through, always allowed.
    ///   - `allow_internal_deny_explicit`   : block deny_cidrs; everyone else passes.
    ///   - `allow_list_only`                : pass only internal + allow_cidrs.
    ///
    /// Unrecognized modes are treated as pass-through (with warn-style trace
    /// behaviour deferred to the caller — this fn is hot-path).
    pub fn check_ip(&self, ip_str: &str) -> IpCheckResult {
        let ip: IpAddr = match ip_str.parse() {
            Ok(addr) => addr,
            Err(_) => {
                return IpCheckResult {
                    allowed: false,
                    reason: "Invalid IP address".to_string(),
                    is_internal: false,
                };
            }
        };

        // Read-lock the policy. Poisoned lock -> fail-open with no-internal
        // semantics (tests would catch a real poison; this is to avoid
        // panicking the request worker).
        let policy = match self.ip_policy.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        if policy.mode == "disabled" {
            return IpCheckResult {
                allowed: true,
                reason: "".into(),
                is_internal: false,
            };
        }

        let is_internal = policy.trusted_internal_cidrs.contains(&ip);

        // Deny list always takes priority — even internal IPs can be denied
        // (useful for surgical revocation in private networks).
        let is_denied = policy.deny_cidrs.contains(&ip);
        if is_denied {
            return IpCheckResult {
                allowed: false,
                reason: "IP address in deny list".to_string(),
                is_internal,
            };
        }

        match policy.mode.as_str() {
            "allow_internal_deny_explicit" => IpCheckResult {
                allowed: true,
                reason: "".into(),
                is_internal,
            },
            "allow_list_only" => {
                if is_internal {
                    return IpCheckResult {
                        allowed: true,
                        reason: "".into(),
                        is_internal: true,
                    };
                }
                let is_allowed = policy.allow_cidrs.contains(&ip);
                if is_allowed {
                    IpCheckResult {
                        allowed: true,
                        reason: "".into(),
                        is_internal: false,
                    }
                } else {
                    IpCheckResult {
                        allowed: false,
                        reason: "IP address not in allow list".to_string(),
                        is_internal: false,
                    }
                }
            }
            _ => IpCheckResult {
                allowed: true,
                reason: "".into(),
                is_internal,
            },
        }
    }

    /// Provision a new key. Generates a random raw key, hashes it, and saves it to the flat JSON config.
    ///
    /// `service_role` (spec §5.6) is immutable: when provided, the same
    /// `key_id` cannot be re-provisioned with a different role. In normal
    /// operation each call generates a fresh `key_id`, so a collision can
    /// only arise from an on-disk config that was hand-edited; the check is
    /// defensive belt-and-braces against that path.
    pub async fn provision_key(
        &self,
        budget_key: &str,
        tier: &str,
        rpm: u32,
        service_role: Option<String>,
    ) -> Result<ProvisionResponse, String> {
        // Serialize file read-modify-write — without this, concurrent provision/revoke
        // calls reading the same on-disk config can drop one another's writes.
        let _guard = self.config_mutex.lock().await;
        use rand_core::{OsRng, RngCore};
        let mut key_bytes = [0u8; 16];
        OsRng.fill_bytes(&mut key_bytes);

        let raw_key = format!("gw_{}", hex::encode(key_bytes));
        let key_hash = self.hash_key(&raw_key);
        // Stable, non-secret identifier derived from the random key bytes.
        // Safe to log / surface in metrics — does not reveal the raw key (8 hex
        // chars of 32-char raw entropy is not invertible to the secret).
        let key_id = format!("k_{}", &hex::encode(key_bytes)[..8]);

        let new_key = AuthKey {
            key_id: key_id.clone(),
            key_hash: key_hash.clone(),
            budget_key: budget_key.to_string(),
            tier: tier.to_string(),
            rate_limit_rpm: rpm,
            rate_limit_burst: 0,
            service_role: service_role.clone(),
        };

        // Load existing config, append, and save
        let (mut config, _) = self.load_config();
        // Defense-in-depth (§5.6): an existing entry with the same `key_id`
        // and a conflicting `service_role` must not be silently overwritten
        // or duplicated. Random key_ids make a collision implausible in
        // normal operation, but a hand-edited config could create one.
        if let Some(existing) = config.keys.iter().find(|k| k.key_id == key_id) {
            if existing.service_role != service_role {
                return Err(format!(
                    "service_role conflict on key_id {} (existing={:?}, requested={:?})",
                    key_id, existing.service_role, service_role
                ));
            }
        }
        config.keys.push(new_key);

        if let Some(path) = &self.config_path {
            let json_str = serde_json::to_string_pretty(&config)
                .map_err(|e| format!("Serialization error: {}", e))?;
            let temp_path = path.with_extension("tmp");
            std::fs::write(&temp_path, &json_str).map_err(|e| format!("IO error: {}", e))?;
            std::fs::rename(&temp_path, path).map_err(|e| format!("Rename error: {}", e))?;
        }

        // Reload into memory
        self.reload().await;

        Ok(ProvisionResponse {
            key_id,
            key_hash,
            raw_key,
            budget_key: budget_key.to_string(),
            tier: tier.to_string(),
        })
    }

    pub async fn revoke_key(&self, key_id: &str) -> Result<bool, String> {
        // Same serialization rationale as provision_key — see above.
        let _guard = self.config_mutex.lock().await;
        let (mut config, _) = self.load_config();
        let original_len = config.keys.len();
        config.keys.retain(|k| k.key_id != key_id);

        if config.keys.len() == original_len {
            return Err(format!("Key '{}' not found", key_id));
        }

        if let Some(path) = &self.config_path {
            let json_str = serde_json::to_string_pretty(&config)
                .map_err(|e| format!("Serialization error: {}", e))?;
            let temp_path = path.with_extension("tmp");
            std::fs::write(&temp_path, &json_str).map_err(|e| format!("IO error: {}", e))?;
            std::fs::rename(&temp_path, path).map_err(|e| format!("Rename error: {}", e))?;
        }

        self.reload().await;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that construct AuthService must guarantee AUTH_PEPPER is set —
    /// otherwise the new fail-closed pepper check panics. Cargo runs tests in
    /// parallel within a process, so env vars are shared; setting the same
    /// value from every test is race-free.
    fn ensure_test_pepper() {
        if std::env::var_os("AUTH_PEPPER").is_none_or(|v| v.is_empty()) {
            std::env::set_var("AUTH_PEPPER", "test_pepper_unit_only");
        }
    }

    #[tokio::test]
    async fn test_provision_and_revoke() {
        ensure_test_pepper();
        let dir = std::env::temp_dir().join(format!("auth_rbac_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let config_path = dir.join("auth_keys.json");
        std::fs::write(&config_path, r#"{"keys":[]}"#).unwrap();

        let service = AuthService::new(Some(config_path.clone()));

        // Provision a key
        let res = service
            .provision_key("tenant_a", "admin", 500, None)
            .await
            .unwrap();
        assert!(!res.raw_key.is_empty());
        assert!(res.key_id.starts_with("k_"));

        // Verify it works
        let check = service.check(&res.raw_key, "127.0.0.1").await;
        assert!(check.allowed);
        assert_eq!(check.tier, "admin");
        assert_eq!(check.key_id, res.key_id);

        // Revoke it
        let revoked = service.revoke_key(&res.key_id).await.unwrap();
        assert!(revoked);

        // Verify it no longer works
        let check2 = service.check(&res.raw_key, "127.0.0.1").await;
        assert!(!check2.allowed);

        // Revoke non-existent key
        let err = service.revoke_key("k_nonexistent").await;
        assert!(err.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_ip_check_internal_addresses() {
        ensure_test_pepper();
        let service = AuthService::new(None);

        let r = service.check_ip("127.0.0.1");
        assert!(r.allowed);
        assert!(r.is_internal);

        let r = service.check_ip("10.0.0.5");
        assert!(r.allowed);
        assert!(r.is_internal);

        let r = service.check_ip("192.168.1.100");
        assert!(r.allowed);
        assert!(r.is_internal);

        let r = service.check_ip("172.16.5.10");
        assert!(r.allowed);
        assert!(r.is_internal);

        // IPv6 loopback
        let r = service.check_ip("::1");
        assert!(r.allowed);
        assert!(r.is_internal);
    }

    #[test]
    fn test_ip_check_external_default_policy() {
        ensure_test_pepper();
        let service = AuthService::new(None);
        // Default mode is allow_internal_deny_explicit with empty deny list:
        // external IPs pass (not denied).
        let r = service.check_ip("8.8.8.8");
        assert!(r.allowed);
        assert!(!r.is_internal);
    }

    #[test]
    fn test_rate_limit_exempt_parsing_and_membership() {
        ensure_test_pepper();
        let service = AuthService::new(None);

        // Default: empty exempt list — nobody is exempt, not even loopback.
        assert!(!service.is_rate_limit_exempt("127.0.0.1"));

        // Bare IPs normalize to host routes; CIDRs parse as-is; garbage drops.
        {
            let mut policy = service.ip_policy.write().unwrap();
            policy.rate_limit_exempt_cidrs = CidrList::from_strs([
                "127.0.0.1",     // bare IPv4 → /32
                "::1",           // bare IPv6 → /128
                "172.16.0.0/12", // proper CIDR
                "not_a_cidr",    // dropped with a warn
            ]);
            assert_eq!(policy.rate_limit_exempt_cidrs.len(), 3);
        }
        assert!(service.is_rate_limit_exempt("127.0.0.1"));
        assert!(!service.is_rate_limit_exempt("127.0.0.2")); // /32, not /8
        assert!(service.is_rate_limit_exempt("::1"));
        assert!(service.is_rate_limit_exempt("172.20.1.1"));
        assert!(!service.is_rate_limit_exempt("8.8.8.8"));
        assert!(!service.is_rate_limit_exempt("not_an_ip"));
    }

    #[test]
    fn test_ip_check_invalid_ip() {
        ensure_test_pepper();
        let service = AuthService::new(None);
        let r = service.check_ip("not_an_ip");
        assert!(!r.allowed);
        assert_eq!(r.reason, "Invalid IP address");
    }

    #[tokio::test]
    async fn test_fail_closed_empty_keys() {
        ensure_test_pepper();
        let service = AuthService::new(None);
        // Default is fail_closed=true, no config file = no keys
        let check = service.check("any_key", "127.0.0.1").await;
        assert!(!check.allowed);
        assert!(check.reason.contains("no keys configured"));
    }
}
