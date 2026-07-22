//! Deliberation engine — the core pipeline
//!
//! fan_out (R1, parallel) → cross_pollinate (R2+, cumulative)
//!   → convergence_score (LLM judge) → Chair synthesis → save_session

pub mod context;
pub mod deliberate;
pub mod direct_fire;
pub mod directive_fence;
pub mod judge_eval;
pub mod sheldon;
pub mod sheldon_eval;

pub use context::RequestContext;
