//! council-rs — Sovereign Intelligence Council
//!
//! Multi-model deliberation engine in Rust.
//! Shared types with the Gateway and the in-tree Librarian subsystem.

pub mod cli;
pub mod config;
pub mod engine;
pub mod evidence;
pub mod governance;
pub mod librarian;
pub mod mapmaker;
pub mod mode;
pub mod precedent;
pub mod provider;
pub mod registry;
pub mod scrub;
pub mod server;
pub mod static_web;
pub mod stream;
pub mod types;
pub mod warroom;

#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests;
