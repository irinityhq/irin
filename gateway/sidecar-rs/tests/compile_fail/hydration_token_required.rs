//! Compile-fail test for P0-β HydrationToken typestate.
//
// This file proves that you cannot call the boot-hydration sweep
// without first obtaining a HydrationToken from DirectiveSigningKey::load_or_initialize.

use gateway_sidecar::watch::db::WatchDb;
use gateway_sidecar::watch::dispatcher::run_boot_hydration_sweep;

fn attempts_hydration_without_token(db: &WatchDb) {
    // This must not compile: calling the hydration function without the token.
    let _future = run_boot_hydration_sweep(db);
    // ^ ERROR: missing HydrationToken argument
}

fn main() {}
