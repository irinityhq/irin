//! Streaming deliberation — the council_stream.py equivalent.
//!
//! This module implements the async event protocol for WebSocket consumption.
//! It re-orchestrates the engine's deliberation loop to yield structured events
//! with pause/resume semantics and operator intervention support.

pub mod deliberate;
pub mod events;
pub mod intervention;
