//! `watch.db` schema and hash-chained writes.
//!
//! INVARIANT (encoded in `insert_fire`):
//!   prev_hash MUST be read INSIDE the same BEGIN IMMEDIATE tx that writes
//!   the new row. OCC check on watch_sentinels.hard_killed_at MUST also
//!   occur in the same tx. No in-memory caching of prev_hash. No pre-fetch
//!   outside the transaction.
//!
//! A frozen distinct genesis hash is the prev_hash of the first fire per
//! tenant — prevents forensic confusion
//! with the Gateway ledger's all-zeros genesis.

mod arming;
mod claims_spend;
mod config;
mod fires;
mod outbox_store;
mod registry;
mod schema;

pub(crate) use arming::read_active_arm_row;
pub use arming::{
    arm_audit_distinct_genesis, compute_arm_audit_preimage, ActiveArmRow, ArmAuditRow,
    ArmConfirmTxOutcome, ArmPendingRow, AttestVerification, PersistedArmSignature,
    ARM_AUDIT_DISTINCT_GENESIS_HASH,
};

pub use claims_spend::PendingClaim;

pub use config::{
    arm_window_ms, arm_window_ms_bootlocked, daily_spend_cap, daily_spend_cap_from_env,
    init_arm_window_ms_at_boot, init_daily_spend_cap_at_boot, init_signed_spend_window_at_boot,
    lease_duration_ms, lease_renew_interval_ms, max_fanout_cost_usd, parse_daily_spend_cap,
    pending_escalations_max_nonterminal, process_instance_uuid, signed_spend_window_enabled,
    utc_day_bucket, watch_distinct_genesis, writer_claim_heartbeat_ms, writer_claim_stale_ms,
    DailySpendCapError, PhantomSweepReport, ReconAlarmRow, RenewOutcome, SettleReport,
    ARM_WINDOW_MS_DEFAULT, DAILY_SPEND_CAP, DAILY_SPEND_CAP_ENV_VAR, LEASE_DURATION_MS_DEFAULT,
    LEASE_RENEW_INTERVAL_MS_DEFAULT, MAX_FANOUT_COST_USD_DEFAULT,
    PENDING_ESCALATIONS_MAX_NONTERMINAL_DEFAULT, WATCH_DISTINCT_GENESIS_HASH,
    WRITER_CLAIM_HEARTBEAT_MS_DEFAULT, WRITER_CLAIM_STALE_MS_DEFAULT,
};

pub(crate) use fires::compute_watch_fire_preimage;
pub use fires::{CommittedFire, FireRow, VerifyBreak, VerifyResult};

pub(crate) use registry::probation_target_for_clear;
pub use registry::{DurableClearOutcome, RegistryRow, TenantPolicy};

use tokio_rusqlite::Connection;

#[derive(Clone)]
pub struct WatchDb {
    conn: Connection,
}
