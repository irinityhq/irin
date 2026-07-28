#![allow(unused_imports)]
//! Installed-release Gateway Pack lifecycle.
//!
//! Privileged native boundary: Docker CLI allow-list, fixed compose project,
//! app-owned paths, Keychain-held GW_API_KEY + AUTH_PEPPER. Renderer only
//! receives non-secret status and triggers fixed workflows.
//!
//! Concurrent lifecycle commands are serialized. Authenticated-ready requires
//! both Gateway auth proof and an owned Council child in the requested route.

pub mod manifest;

mod enable;
mod env;
mod health;
mod install;
mod keys;
mod launch;
mod paths;
mod status;
mod types;

// --- Public surface (unchanged from monolithic mod.rs) ---

pub use types::{GatewayPackState, GatewayPackStatus, SUPPORT_MATRIX_SUMMARY};

#[cfg(test)]
pub use status::status_cache_generation_for_test;
pub use status::{
    bump_pack_lifecycle_generation, gateway_pack_status, gateway_pack_status_fresh,
    invalidate_status_cache, owned_council_route, pack_lifecycle_generation,
    record_owned_council_route,
};

pub use paths::{
    arm_keys_path, bundled_pack_root, gateway_data_dir, installed_marker_path, is_pack_installed,
    ledger_key_path, public_env_path, runtime_env_path, ARM_KEYS_CONTAINER_PATH,
};

pub use keys::{ensure_arm_keys_file, serialize_public_env, validate_env_value};

pub use install::{install_pack_files, installed_pack_root};

pub use enable::{
    disable_gateway_pack, enable_gateway_pack, lifecycle_stage, stop_gateway_pack,
    uninstall_gateway_pack,
};

#[cfg(test)]
pub use launch::{
    cold_launch_owned_via_gateway, decide_launch_resume_outcome, decide_launch_via_gateway,
    frontend_may_start_council, LaunchResumeOutcome,
};
pub use launch::{
    default_secret_store, gateway_child_env_if_ready, may_promote_to_governed,
    pack_auth_revalidated, resume_installed_pack, status_with_council_route, GatewayChildEnv,
};
