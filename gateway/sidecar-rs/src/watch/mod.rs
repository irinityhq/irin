//! Phase 2 watch plane — Sentinel trait + supporting types.
//! See spec §6.1 + §13 wall-line discipline.
//!
//! Trait signature has no LLM parameter — a sentinel needing an LLM is a
//! Worker, caught at compile time, not lint time.
//!
//! Submodules (runtime, quarantine, db) land incrementally in Tasks 9-11.

pub mod api;
pub mod attest; // dual-custody-local-attest (spec §5) — arm-confirm challenge + boot self-test
pub mod db;
pub mod dispatcher; // C11 / Fork 1 — council-triage header construction
pub mod outbox;
pub mod quarantine;
pub mod recon; // watch telemetry — out-of-band spend reconciliation (telemetry invariant)
pub mod registry;
pub mod runner;
pub mod runtime;
pub mod sentinels;
pub mod startup_probe; // Phase 3a cabinet schema probe (AC-19h / startup gate)
pub mod worker;

pub mod fire_identity; // P0-D causal_fire_id primitive (pure, per causal_fire_id.md §3 + Duplicate Fire Collapse test)

use async_trait::async_trait;
use std::time::Duration;

/// Sub-budget tier. Sentinel author declares which tier their `observe()` falls
/// under; the runner uses this for metric labeling, not budget enforcement.
/// (Budget enforcement is the §8.2 pipeline's job and is the same regardless.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Fast,
    Polling,
    Deep,
}

pub use sovereign_protocol::{Escalation, SentinelState, Urgency};

#[derive(Debug, thiserror::Error)]
pub enum ObserveError {
    #[error("transient upstream: {0}")]
    TransientUpstream(String),
    #[error("config error: {0}")]
    Config(String),
    #[error("fatal: {0}")]
    Fatal(String),
}

#[derive(Debug, thiserror::Error)]
pub enum EscalateError {
    #[error("transient: {0}")]
    Transient(String),
    #[error("fatal: {0}")]
    Fatal(String),
}

#[async_trait]
pub trait Sentinel: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn tenant(&self) -> &str;
    fn tier(&self) -> Tier;
    fn cooldown(&self) -> Duration;

    async fn observe(&self) -> Result<SentinelState, ObserveError>;
    fn interesting(&self, state: &SentinelState) -> Option<String>;
    async fn escalate(
        &self,
        state: SentinelState,
        reason: String,
    ) -> Result<Escalation, EscalateError>;
}
