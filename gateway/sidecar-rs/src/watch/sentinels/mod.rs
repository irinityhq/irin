//! Built-in Sentinel implementations.
//!
//! The registry exposes file inbox, silence, queue depth, watch health,
//! ledger delta, sequence watch, anomaly, completion verification, and precedent integrity.

pub mod anomaly;
pub mod completion_verify;
pub mod file_inbox;
pub mod ledger_delta;
pub mod precedent_integrity;
pub mod queue_depth;
pub mod sequence_watch;
pub mod silence;
pub mod watch_health;
