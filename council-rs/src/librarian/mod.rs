//! Librarian backend for the War Room page.
//!
//! Upstream endpoint: `LIBRARIAN_BASE_URL` (default `http://127.0.0.1:11435`).
//! Storage: `librarian_chats/` at repo root (gitignored). IDs match
//! `^lib_\d{4}-\d{2}-\d{2}_[a-z0-9]{6}$`.

pub mod adapter;
pub mod cabinets;
pub mod health;
pub mod idempotency;
pub mod redaction;
pub mod routes;
pub mod storage;
pub mod title;
