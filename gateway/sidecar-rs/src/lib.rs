// ==========================================================================
// lib.rs — minimal library surface for the gateway-sidecar crate.
//
// Exists so out-of-tree binaries (gateway-ceremony, future tooling) can call
// shared modules without copy-pasting their logic. Keep this surface small —
// the sidecar's HTTP handlers stay in main.rs and are NOT re-exported here.
//
// `keymgmt` imports `EVENT_KEY_INTRODUCE` / `EVENT_KEY_REVOKE` from `ledger`
// so we expose `ledger` too — the binaries don't actually call into the
// async/SQLite parts, but the constants need to resolve at compile time.
// ==========================================================================

pub mod auth;
pub mod comms;
pub mod council_storage;
pub mod keymgmt;
pub mod ledger;
pub mod socket;
pub mod watch;
