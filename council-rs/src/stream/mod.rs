//! REST and CLI deliberation use `crate::engine::deliberate::run_with_cancel`.
//! WebSocket deliberation uses `crate::stream::deliberate::run`, which imports engine helpers and adds events, pause/resume, and interventions.

pub mod deliberate;
pub mod events;
pub mod intervention;
