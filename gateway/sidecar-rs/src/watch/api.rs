//! Watch-plane HTTP handlers.
//!
//! `GET /watch/verify-chain/:tenant` is bounded by a 5s timeout and returns
//! the `WatchDb::verify_chain` result as JSON.
//!
//! Handlers in here are deliberately *not* coupled to `AppState`. Each one
//! takes the narrowest state it actually needs (e.g. `Arc<WatchDb>`) and
//! returns a `(StatusCode, Json<Value>)` tuple. main.rs wraps them in a
//! thin extractor-bound function that pulls the right field out of
//! `AppState`. This keeps the library crate buildable in isolation because
//! `AppState` lives only in the binary crate.
// -----------------------------------------------------------------------------
// APPEND-ONLY INVARIANT
// The watch_fires audit chain is strictly append-only.
// You MUST NOT author any HTTP endpoints or API routes that delete,
// truncate, compact, or prune the watch audit chain.
// -----------------------------------------------------------------------------

mod helpers;

pub use helpers::FORCE_WAKE_DEFAULT_TENANT;
// Re-export pub(crate) surface for binary/integration paths; not all are
// named inside this facade after the helpers extraction.
#[allow(unused_imports)]
pub(crate) use helpers::{admin_token_matches, CANARY_TENANT_DEFAULT, CANARY_TENANT_ENV_VAR};
pub use helpers::{resolve_canary_from, resolve_canary_tenant};

mod status;

pub use status::{list_json, temperature_json, verify_chain_json, VERIFY_CHAIN_BUDGET};

mod stats;

pub use stats::{
    audit_json, build_watch_stats, ui_snapshot_json, UiRecentFire, UiSentinelReadiness,
    UiWatchBudget, UiWatchDegradation, UiWatchSnapshot, UiWatchTemperature, WatchStats,
    AUDIT_LIMIT_CAP, AUDIT_LIMIT_DEFAULT,
};

mod force_wake;

pub use force_wake::{
    clear_quarantine_json, force_wake_json, ForceWakeRegistry, DELETE_QUARANTINE_DEFAULT_TENANT,
    FORCE_WAKE_DEFAULT_REASON,
};

mod outbox_admin;

pub use outbox_admin::{
    ack_outbox_json, claim_outbox_json, get_outbox_json, heartbeat_outbox_json, list_outbox_json,
    nack_outbox_json, outbox_pubkey_json, watch_get_tenant_policy, watch_set_tenant_policy,
    worker_ack_outbox_json, ClaimRequest, HeartbeatRequest, NackRequest, WorkerAckRequest,
};

mod arming;
mod writer_claim;

pub use writer_claim::{writer_claim_heartbeat_loop, writer_claim_heartbeat_step};

pub use arming::{
    admin_arm_confirm_json, admin_arm_pending_json, admin_arm_producer_json, admin_arm_stage_json,
    admin_disarm_producer_json, arm_admin_router, arm_stage_ttl, auto_disarm_producer,
    ArmAdminRouterState, ArmDeviationTags, ArmNotifier, ArmPrincipals, ARM_STAGE_TTL_MS_DEFAULT,
};
