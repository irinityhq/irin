// ==========================================================================
// main.rs — Gateway sidecar HTTP server.
//
// Axum-based replacement for Python FastAPI sidecar.
// Endpoints:
//   POST /guard/input   — prompt injection + encoding attack scanning
//   POST /guard/scan    — DEBUG ONLY (GATEWAY_DEBUG_GUARD_SCAN=1; 404 otherwise):
//                         raw internal decontaminator scan struct
//   POST /guard/tool    — tool call authorization (READ_ONLY allowlist)
//   POST /cache/check   — response cache lookup
//   POST /cache/store   — response cache write
//   POST /route/decide  — smart routing decision
//   POST /route/outcome — record provider response for health tracking
//   POST /budget/check  — pre-flight budget gate
//   POST /budget/record — post-flight spend recording
//   POST /policy/evaluate — sensitivity-based provider filtering
//   GET  /health        — sidecar health check
// ==========================================================================

mod auth;
mod boot;
mod budget;
mod cache;
pub mod comms;
pub mod council;
pub mod council_storage;
mod decontaminator;
mod enforcer;
mod keymgmt;
mod ledger;
mod policy;
mod ratelimit;
mod router;
mod routes;
mod socket;
mod sovereignty_gate;
mod unified_config;
mod vertex_auth;
pub mod watch;

use std::os::unix::fs::PermissionsExt;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

pub(crate) struct AppState {
    decon: decontaminator::InputDecontaminator,
    cache: cache::GatewayCache,
    router: router::SmartRouter,
    budget: budget::BudgetEnforcer,
    policy: policy::PolicyFirewall,
    sovereignty: sovereignty_gate::SovereigntyGate,
    ledger: ledger::AuditLedger,
    ledger_signing_key: ed25519_dalek::SigningKey,
    /// Air-gapped root verifying key, loaded from ROOT_PUBKEY_HEX at startup.
    /// When `Some`, ceremony events (key_introduce/key_revoke) signed by the
    /// root are verifiable; when `None`, root verification is skipped with a
    /// warning at startup (backward compatibility).
    #[allow(dead_code)]
    root_pubkey: Option<ed25519_dalek::VerifyingKey>,
    auth: auth::AuthService,
    vertex_token: vertex_auth::VertexTokenProvider,
    /// Council endpoint per-key concurrency + in-memory idempotency cache
    /// (spec §5.8). In-memory only in v0.1 — a sidecar restart loses replay
    /// history (startup WARN emitted). SQLite-backed in v0.1.1.
    pub council: council::CouncilState,
    /// Phase 2 watch.db handle — append-only hash-chained fire log per
    /// tenant. Powers T31 `/watch/verify-chain/:tenant` and the upcoming
    /// `/watch/list` / `/watch/audit` endpoints. Opened at boot; the
    /// chain itself is written via `QuarantineState::write_fire_row` →
    /// `WatchDb::insert_fire` once sentinels start firing.
    pub watch_db: std::sync::Arc<watch::db::WatchDb>,
    /// Phase 2 T30 — (tenant, sentinel_name) → sentinel handle, populated
    /// from `sentinels.yaml` at boot. Powers `POST /watch/force-wake/{sentinel}`.
    pub watch_registry: watch::api::ForceWakeRegistry,
    /// Phase 2 — in-memory quarantine state (hysteresis + hard-kill).
    /// Force-wake gates on this before jumping to escalate().
    pub watch_quarantine: std::sync::Arc<watch::quarantine::QuarantineState>,
    /// Resolved at boot: WATCH_ADMIN_TOKEN || BOOTSTRAP_TOKEN. Empty → all
    /// force-wake requests fail closed with 401 (constant-time compare).
    pub watch_admin_token: String,
    /// Wave-1 single-tenant tripwire: the ONE tenant the outbox surface
    /// accepts. Resolved ONCE at boot from `WATCH_CANARY_TENANT` (default
    /// "sovereign") via `watch::api::resolve_canary_tenant`; the guard compares
    /// every resolved tenant scope against this configured value, not a
    /// hardcoded const. Set only in the CI/phase-3-smoke sidecar; local canary
    /// stays "sovereign".
    pub watch_canary_tenant: String,
    // p0a-four-eyes arm principal registry + stage TTL moved into
    // `watch::api::ArmAdminRouterState` (the arm routes
    // live in the lib crate so the wiring is oneshot-tested).
    /// Librarian upstream url for identity/memory proxy and commits
    pub librarian_base_url: String,
}

// ---------------------------------------------------------------------------
// Ledger signing key loader
//
// Loads the Ed25519 signing key seed from disk. The file is exactly 32 raw
// bytes (despite the historical `.pem` extension on the default path — the
// extension is misleading; the contents are a raw seed, accidentally
// compatible with `ed25519-dalek::SigningKey::from_bytes`).
//
// Fails closed: missing file, wrong size, or non-0600 permissions panic
// at startup. No ephemeral key generation — that would silently break
// chain verification across restarts.
//
// See COUNCIL_GATEWAY_CONTRACT.md for the trust root section.
// ---------------------------------------------------------------------------
fn load_ledger_signing_key() -> Vec<u8> {
    let key_path = std::env::var("LEDGER_SIGNING_KEY_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME")
                .expect("FATAL: HOME env var must be set to locate the ledger signing key");
            std::path::PathBuf::from(home)
                .join(".irin")
                .join("ledger_key.pem")
        });

    let metadata = std::fs::metadata(&key_path).unwrap_or_else(|e| {
        panic!(
            "FATAL: cannot stat ledger signing key at {:?}: {}. \
             Set LEDGER_SIGNING_KEY_PATH or place a 32-byte seed file at the default path.",
            key_path, e
        )
    });

    let perms = metadata.permissions().mode() & 0o777;
    if perms != 0o600 {
        panic!(
            "FATAL: ledger signing key at {:?} must be chmod 0600 (got {:o}). \
             Run: chmod 600 {:?}",
            key_path, perms, key_path
        );
    }

    let bytes = std::fs::read(&key_path).unwrap_or_else(|e| {
        panic!(
            "FATAL: cannot read ledger signing key at {:?}: {}",
            key_path, e
        )
    });

    if bytes.len() != 32 {
        panic!(
            "FATAL: ledger signing key at {:?} must be exactly 32 bytes (got {}). \
             Generate with: openssl rand -out {:?} 32 && chmod 600 {:?}",
            key_path,
            bytes.len(),
            key_path,
            key_path
        );
    }

    info!(path = %key_path.display(), "ledger signing key loaded");
    bytes
}

// ---------------------------------------------------------------------------
// Root verifying key loader (ROOT_PUBKEY_HEX)
//
// The air-gapped root signing key never reaches a running sidecar — only its
// public counterpart is needed for verification. When `ROOT_PUBKEY_HEX` is
// set to a 64-character hex string (32 raw Ed25519 public-key bytes), it is
// parsed into a `VerifyingKey` and held on AppState. This enables ceremony
// (key_introduce / key_revoke) envelope verification at fsck/verify time.
//
// When unset or unparseable, the function returns `None` and we log a warning
// — the sidecar continues to start so existing deployments keep working.
// Operators graduating to PKI provide the hex; everyone else stays on the
// implicit-root model with the active signing key as the de-facto root.
// ---------------------------------------------------------------------------
fn load_root_pubkey() -> Option<ed25519_dalek::VerifyingKey> {
    let hex_str = match std::env::var("ROOT_PUBKEY_HEX") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => {
            warn!("ROOT_PUBKEY_HEX not set — ceremony envelope root-verification disabled");
            return None;
        }
    };
    if hex_str.len() != 64 {
        warn!(
            len = hex_str.len(),
            "ROOT_PUBKEY_HEX must be exactly 64 hex chars (32 bytes) — root verification disabled"
        );
        return None;
    }
    let bytes = match hex::decode(&hex_str) {
        Ok(b) => b,
        Err(e) => {
            warn!(
                "ROOT_PUBKEY_HEX is not valid hex ({}) — root verification disabled",
                e
            );
            return None;
        }
    };
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    match ed25519_dalek::VerifyingKey::from_bytes(&arr) {
        Ok(vk) => {
            info!(pubkey = %hex_str, "ROOT_PUBKEY_HEX loaded — ceremony root verification enabled");
            Some(vk)
        }
        Err(e) => {
            warn!("ROOT_PUBKEY_HEX bytes do not form a valid Ed25519 point ({}) — root verification disabled", e);
            None
        }
    }
}

fn load_old_ledger_key() -> Option<Vec<u8>> {
    // Apply the same strict file checks to the primary and previous keys.
    // to the old-key path used in dual-signing window / ceremony. Previously silent-ignore on bad file.
    // If the env var is set, the file MUST be valid — fail closed for provenance hygiene.
    if let Ok(path) = std::env::var("LEDGER_OLD_SIGNING_KEY_PATH") {
        let key_path = std::path::PathBuf::from(path);
        let metadata = std::fs::metadata(&key_path).unwrap_or_else(|e| {
            panic!(
                "FATAL: LEDGER_OLD_SIGNING_KEY_PATH set but cannot stat {:?}: {}. \
                 Must be 32-byte 0600 seed during rotation window. \
                 Set LEDGER_OLD_SIGNING_KEY_PATH or place a 32-byte seed file at the path.",
                key_path, e
            )
        });
        let perms = metadata.permissions().mode() & 0o777;
        if perms != 0o600 {
            panic!(
                "FATAL: LEDGER_OLD_SIGNING_KEY_PATH at {:?} must be chmod 0600 (got {:o}). \
                 Run: chmod 600 {:?}",
                key_path, perms, key_path
            );
        }
        let bytes = std::fs::read(&key_path).unwrap_or_else(|e| {
            panic!(
                "FATAL: cannot read LEDGER_OLD_SIGNING_KEY_PATH at {:?}: {}",
                key_path, e
            )
        });
        if bytes.len() != 32 {
            panic!(
                "FATAL: LEDGER_OLD_SIGNING_KEY_PATH at {:?} must be exactly 32 bytes (got {}). \
                 Generate with: openssl rand -out {:?} 32 && chmod 600 {:?}",
                key_path,
                bytes.len(),
                key_path,
                key_path
            );
        }
        info!(path = %key_path.display(), "old ledger signing key loaded for rotation window");
        return Some(bytes);
    }
    None
}

// ---------------------------------------------------------------------------
// Entry point — orchestration outline
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Telemetry (JSON logs + optional OTEL)
    let otel_provider = boot::init_telemetry();

    // 2–5. Config / state / router / serve (single stage keeps move pure)
    boot::load_config_build_state_and_serve(otel_provider).await
}
