//! Inter-product comms types (Sentinel ↔ Gateway ↔ Council ↔ Worker).
//!
//! The contract spine lives in `sentinel/COMMS_CONTRACT.md` (v0.1). The
//! CloudEvents 1.0 envelope carries the escalation to directive to outbox loop.

pub mod envelope;
