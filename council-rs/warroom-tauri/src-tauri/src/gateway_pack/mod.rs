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

mod cli_adapters;
mod enable;
mod env;
mod health;
mod install;
mod keys;
mod launch;
mod paths;
mod status;
mod types;
mod watch_profile;

pub use types::{GatewayPackState, GatewayPackStatus, SUPPORT_MATRIX_SUMMARY};

pub(crate) use status::seed_auth_observation_from_preloaded_key;
#[cfg(test)]
pub use status::status_cache_generation_for_test;
pub use status::{
    auth_observation_generation, bump_pack_lifecycle_generation, gateway_pack_status,
    gateway_pack_status_fresh, gateway_pack_status_fresh_with_key, invalidate_auth_observation,
    invalidate_status_cache, owned_council_route, pack_lifecycle_generation,
    record_owned_council_route,
};
#[cfg(test)]
pub use status::lifecycle_gen_test_lock;

pub use paths::{
    arm_keys_path, bundled_pack_root, ensure_watch_dirs, gateway_data_dir, installed_marker_path,
    is_pack_installed, ledger_key_path, public_env_path, runtime_env_path, sentinels_dir,
    watch_inbox_dir, watch_profile_path, ARM_KEYS_CONTAINER_PATH, WATCH_PROFILE_CONTAINER_PATH,
};

pub use keys::{ensure_arm_keys_file, serialize_public_env, validate_env_value};

pub(crate) use env::PACK_WATCH_CANARY_TENANT;
pub use env::{load_launch_secrets, LaunchSecrets};

pub use install::{install_pack_files, installed_pack_root};

pub use enable::{
    disable_gateway_pack, enable_gateway_pack, lifecycle_stage, stop_gateway_pack,
    uninstall_gateway_pack,
};

pub use watch_profile::{
    open_watch_inbox, set_watch_sentinels_enabled, watch_inbox_path_string, watch_sentinels_enabled,
};

#[cfg(test)]
pub use launch::{
    cold_launch_owned_via_gateway, decide_launch_resume_outcome, decide_launch_via_gateway,
    frontend_may_start_council, LaunchResumeOutcome,
};
pub use launch::{
    classify_post_pack_promote_decision, default_secret_store, evaluate_promote_flight_attempt,
    governed_launch_after_watch_reconciliation, may_promote_to_governed,
    pack_auth_revalidated, pack_auth_revalidated_with_key, promote_after_stop_lifecycle_recovery,
    promote_commit_after_stop_wait_detailed, promote_held_secrets_still_valid,
    promote_may_call_resume, promote_may_commit_after_pack_ready, promote_pack_ready_for_attempt,
    promote_port_release_target, resume_installed_pack, resume_installed_pack_with_key,
    status_with_council_route, status_with_council_route_with_key,
    watch_admin_surfaces_authenticated, AfterStopLifecycleRecovery, PromoteCommitError,
    PromoteFlightDecision, PromotePackAttempt,
};

#[cfg(test)]
pub use launch::{decide_resume_pack_action, ResumePackAction};

/// App-owned Claude/Codex host-adapter lifecycle (DMG path).
pub use cli_adapters::{
    current_status as cli_adapters_status, ensure_cli_adapters, restart_cli_adapters,
    stop_cli_adapters, AdapterHealth, CliAdaptersStatus,
};
