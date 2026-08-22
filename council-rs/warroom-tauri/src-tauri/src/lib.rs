// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/

#[cfg(test)]
#[path = "../build_support.rs"]
mod build_support;
mod docker_cli;
mod gateway_pack;
mod keychain;
mod lifecycle;
mod paths;
mod phone_access;
mod private_config;
mod sidecar;
mod status_authority;
mod tailscale_cli;
mod touch_id;

use gateway_pack::{GatewayPackState, GatewayPackStatus};
use keychain::{
    load_gw_api_key, load_watch_admin_token, migrate_legacy_secrets_with_values,
    seed_arm_principal_observation, KeychainSecretStore,
};
use lifecycle::{
    classify_council_lifecycle, classify_gateway_lifecycle, classify_phone_lifecycle,
    compose_app_lifecycle, AppLifecycleStatus, CouncilLifecycleInput,
};
use paths::{
    build_cors_origins, default_serve_port, is_packaged_install, resolve_council_binary,
    resolve_council_rs_dir, resolve_spawn_base_dir, validate_serve_port,
};
use phone_access::{LiveTailscaleRunner, PhoneAccessStatus};
use private_config::{
    ensure_writable_base_overlay, gui_login_environment, load_or_create_private_config,
};
use sidecar::{
    compose_sidecar_args, compose_sidecar_env, probe_council_server, wait_for_port_release,
    CouncilServerProbe, GatewayChildCredentials,
};
use status_authority::{DesktopStatusSnapshot, Freshness};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Fail-closed owned Council PID for process-exit reclaim (SIGTERM/atexit).
/// 0 means no owned child is recorded.
static OWNED_COUNCIL_PID: AtomicU32 = AtomicU32::new(0);
use tauri::{
    menu::{Menu, MenuItem},
    tray::{TrayIconBuilder, TrayIconEvent},
    webview::PageLoadEvent,
    AppHandle, Emitter, Manager, RunEvent, State,
};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_shell::{
    process::{CommandChild, CommandEvent},
    ShellExt,
};

/// Tracked child for the spawned council --serve process. `generation` ties the
/// log pump's Terminated cleanup to the spawn that created it, so a stale
/// Terminated event from a killed child cannot clear a freshly respawned child.
#[derive(Default)]
struct TrackedChild {
    generation: u64,
    child: Option<CommandChild>,
}

/// Tracked server state for the spawned council --serve process.
struct CouncilServer(Mutex<TrackedChild>);

/// Last spawn config, cached so `restart_sidecar` can respawn with the same
/// council path, pairing token, and council root (the frontend only passes the
/// token on start; without this cache a release-build restart would silently
/// drop auth — and a gateway-toggle restart would silently drop `--base-dir`).
#[derive(Default, Clone)]
struct LastSpawnConfig {
    server_port: Option<u16>,
    auth_token: Option<String>,
    librarian_base: Option<String>,
}

struct SpawnConfigCache(Mutex<LastSpawnConfig>);

/// Captured stdout/stderr from the council server process (for "Backend" tab).
struct ServerLogs(Mutex<Vec<String>>);

fn bundled_build_identity() -> (&'static str, bool) {
    (
        env!("IRIN_TAURI_BUILD_GIT_SHA"),
        env!("IRIN_TAURI_BUILD_DIRTY") == "true",
    )
}

fn desktop_runtime_mode_value() -> &'static str {
    if cfg!(debug_assertions) {
        "development"
    } else {
        "installed-release"
    }
}

#[tauri::command]
fn desktop_runtime_mode() -> &'static str {
    desktop_runtime_mode_value()
}

/// Packaged installs: native setup is the sole Council startup owner.
/// Source-dev (unpackaged) returns false so the frontend may still start.
#[tauri::command]
fn native_owns_council_startup() -> bool {
    is_packaged_install()
}

fn desktop_runtime_config_value(port: u16) -> serde_json::Value {
    serde_json::json!({
        "apiBase": format!("http://127.0.0.1:{port}"),
        "wsBase": format!("ws://127.0.0.1:{port}"),
    })
}

#[tauri::command]
fn desktop_runtime_config() -> Result<serde_json::Value, String> {
    let port = default_serve_port()?;
    eprintln!("[runtime-config] selected Council port: {port}");
    Ok(desktop_runtime_config_value(port))
}

fn validate_runtime_ready_port(port: u16, expected: u16) -> Result<(), String> {
    if port == expected {
        Ok(())
    } else {
        Err(format!(
            "webview reported Council port {port}, expected {expected}"
        ))
    }
}

#[tauri::command]
fn report_council_runtime_ready(port: u16) -> Result<(), String> {
    let expected = default_serve_port()?;
    validate_runtime_ready_port(port, expected)?;
    eprintln!("[runtime-config] webview Council requests ready on :{port}");
    Ok(())
}

fn show_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        eprintln!("[tray] main War Room window is unavailable");
        return;
    };
    if let Err(error) = window.show() {
        eprintln!("[tray] failed to show War Room window: {error}");
        return;
    }
    if let Err(error) = window.set_focus() {
        eprintln!("[tray] failed to focus War Room window: {error}");
    }
}

fn should_reveal_main_window(webview_label: &str, event: PageLoadEvent) -> bool {
    webview_label == "main" && event == PageLoadEvent::Finished
}

/// Best-effort kill of the tracked council sidecar (shared by stop command, tray, and app exit).
///
/// Prefer an orderly kill of the owned child, then re-check the PID so a stuck
/// listener is not left reparented under launchd/PID 1 when the host exits.
/// Never kills a process that is not the tracked child PID.
fn record_owned_council_pid(pid: Option<u32>) {
    OWNED_COUNCIL_PID.store(pid.unwrap_or(0), Ordering::SeqCst);
}

fn kill_recorded_owned_council_pid() {
    let pid = OWNED_COUNCIL_PID.swap(0, Ordering::SeqCst);
    if pid == 0 {
        return;
    }
    unix_kill_pid(pid, 15);
    std::thread::sleep(Duration::from_millis(150));
    if unix_pid_alive(pid) {
        unix_kill_pid(pid, 9);
    }
}

extern "C" fn atexit_kill_owned_council() {
    kill_recorded_owned_council_pid();
}

fn stop_tracked_council_server(app: &AppHandle) {
    let state = app.state::<CouncilServer>();
    let mut tracked_pid: Option<u32> = None;
    if let Ok(mut guard) = state.0.lock() {
        if let Some(child) = guard.child.take() {
            tracked_pid = Some(child.pid());
            let _ = child.kill();
        }
    };
    // Prefer the last owned spawn port; fall back to the build-time default.
    let owned_port = app
        .try_state::<SpawnConfigCache>()
        .and_then(|cache| cache.0.lock().ok().and_then(|g| g.server_port))
        .or_else(|| default_serve_port().ok())
        .unwrap_or(8765);
    // Always clear the process-global owned PID so atexit does not double-kill.
    if let Some(pid) = tracked_pid {
        record_owned_council_pid(None);
        // No owned child anymore: clear the governed-route proof so pack
        // status cannot claim governed from health + persisted flag alone.
        gateway_pack::record_owned_council_route(None);
        // Give the child a moment to exit after kill(); then SIGTERM/SIGKILL
        // only if that exact PID is still alive (fail-closed owned reclaim).
        std::thread::sleep(Duration::from_millis(150));
        if unix_pid_alive(pid) {
            unix_kill_pid(pid, 15);
            std::thread::sleep(Duration::from_millis(200));
            if unix_pid_alive(pid) {
                unix_kill_pid(pid, 9);
            }
        }
        // Best-effort listener release after owned child death (actual port).
        let _ = wait_for_port_release(owned_port, Duration::from_secs(3));
    } else {
        // Host exit without a tracked CommandChild handle: still reclaim the
        // recorded PID if any (race between drop and Exit).
        kill_recorded_owned_council_pid();
        let _ = wait_for_port_release(owned_port, Duration::from_secs(3));
    }
}

#[cfg(unix)]
fn unix_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // kill(pid, 0) probes existence without signaling.
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn unix_pid_alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn unix_kill_pid(pid: u32, sig: i32) {
    if pid == 0 {
        return;
    }
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let _ = unsafe { kill(pid as i32, sig) };
}

#[cfg(not(unix))]
fn unix_kill_pid(_pid: u32, _sig: i32) {}

/// Bounded post-launch promote: when cold-start fell to Direct while
/// `via_gateway_default` stayed true, retry pack resume + governed respawn
/// without requiring a manual Enable click. Fail-closed: a failed promote
/// restores Direct; never invents governed readiness.
///
/// `held_gw_key` + `launch_secrets` are the cold-launch Keychain flight values.
/// When present, promote revalidates/resumes/spawns without re-entering Keychain
/// for those six accounts (each get can surface a macOS ACL dialog under ad-hoc
/// signatures). Absent values fall back to the legacy load path.
///
/// Credentials are tied to `pack_lifecycle_generation` at schedule time: if
/// Enable/disable/uninstall advances generation (and may rotate the GW key),
/// later attempts abort rather than resume/spawn with a stale snapshot.
fn schedule_governed_promote_attempts(
    app: AppHandle,
    auth_token: Option<String>,
    held_gw_key: Option<String>,
    launch_secrets: Option<gateway_pack::LaunchSecrets>,
) {
    const ATTEMPTS: u32 = 12;
    const INTERVAL: Duration = Duration::from_secs(5);
    /// Early window may call resume; resume is single-flight and wait-only when
    /// the pack project is already up (no repeated force-recreate/Keychain rebuild).
    const MAX_EARLY_RESUME_ATTEMPTS: u32 = 4;
    let lifecycle_at_schedule = gateway_pack::pack_lifecycle_generation();
    let auth_gen_at_schedule = gateway_pack::auth_observation_generation();
    tauri::async_runtime::spawn_blocking(move || {
        for attempt in 0..ATTEMPTS {
            std::thread::sleep(INTERVAL);
            let store = KeychainSecretStore;
            let persisted = match load_or_create_private_config() {
                Ok(cfg) => cfg.via_gateway_default,
                Err(_) => return,
            };
            let owned = gateway_pack::owned_council_route();
            // Launch-owned flight decision (lifecycle fence + pack readiness +
            // post-pack recheck). Shell only acts on ReadyToPromote.
            match gateway_pack::evaluate_promote_flight_attempt(
                &store,
                lifecycle_at_schedule,
                held_gw_key.as_deref(),
                launch_secrets.as_ref(),
                attempt,
                MAX_EARLY_RESUME_ATTEMPTS,
                persisted,
                owned,
            ) {
                gateway_pack::PromoteFlightDecision::AbortLifecycleChanged => {
                    let _ = app.emit(
                        "council-log",
                        "[system] governed-promote: pack lifecycle changed; aborting held-secret promote",
                    );
                    return;
                }
                gateway_pack::PromoteFlightDecision::StopNotEligible => return,
                gateway_pack::PromoteFlightDecision::WaitNotReady { reason } => {
                    if let Some(reason) = reason {
                        let _ = app.emit(
                            "council-log",
                            format!(
                                "[system] governed-promote attempt {}: pack not ready ({reason})",
                                attempt + 1
                            ),
                        );
                    }
                    continue;
                }
                gateway_pack::PromoteFlightDecision::ReadyToPromote => {}
            }
            let config = {
                let state = app.state::<SpawnConfigCache>();
                let guard = match state.0.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                let cloned = guard.clone();
                drop(guard);
                cloned
            };
            let token = auth_token
                .as_deref()
                .or(config.auth_token.as_deref())
                .map(str::to_string);
            let preloaded_gateway_creds = match (held_gw_key.as_ref(), launch_secrets.as_ref()) {
                (Some(api_key), Some(secrets)) => Some(GatewayChildCredentials {
                    api_key: api_key.clone(),
                    gateway_url: docker_cli::DESKTOP_GATEWAY_URL.to_string(),
                    watch_admin_token: Some(secrets.watch_admin_token.clone()),
                }),
                _ => None,
            };
            // Commit boundary: stop + port wait + generation recheck + spawn
            // are one helper so enable/disable cannot advance generation in the
            // gap between fence and governed start (Codex residual).
            let app_for_stop = app.clone();
            let app_for_start = app.clone();
            let port = config.server_port;
            let token_owned = token.clone();
            let creds = preloaded_gateway_creds.clone();
            let librarian = config.librarian_base.clone();
            // Wait on the same port the restart will bind (`config.server_port`),
            // not only the build-time default — otherwise a non-default-port app
            // skips the real wait and reintroduces the bind race.
            let wait_port = gateway_pack::promote_port_release_target(
                port,
                default_serve_port().unwrap_or(8765),
            );
            match gateway_pack::promote_commit_after_stop_wait_detailed(
                lifecycle_at_schedule,
                || stop_tracked_council_server(&app_for_stop),
                || {
                    let _ = wait_for_port_release(wait_port, Duration::from_secs(5));
                },
                || {
                    try_start_council_server_with_credentials(
                        &app_for_start,
                        port,
                        token_owned.as_deref(),
                        Some(true),
                        librarian.as_deref(),
                        creds.as_ref(),
                    )
                },
            ) {
                Ok(msg) => {
                    let _ = app.emit("council-log", format!("[system] governed-promote: {msg}"));
                    let _ = gateway_pack::status_with_council_route_with_key(
                        &store,
                        held_gw_key.as_deref(),
                        true,
                        false,
                    );
                    // Held-key path: re-seed presentation from the snapshot
                    // (no Keychain re-get) + Background recompute.
                    // Legacy fallback (no held key): status above may have
                    // loaded a key via Keychain; never seed None (that would
                    // cache false unauthenticated). Action recompute owns
                    // presentation after a successful unheld promote.
                    if held_gw_key.is_some() {
                        gateway_pack::seed_auth_observation_from_preloaded_key(
                            held_gw_key.as_deref(),
                            auth_gen_at_schedule,
                        );
                        let _ = status_authority::recompute(&app, Freshness::Background);
                    } else {
                        let _ = status_authority::recompute(&app, Freshness::Action);
                    }
                    return;
                }
                Err(gateway_pack::PromoteCommitError::LifecycleChangedBeforeStop) => {
                    // Stop never ran — leave the pre-flight child alone and end
                    // this held-secret flight (generation advanced elsewhere).
                    let _ = app.emit(
                        "council-log",
                        "[system] governed-promote: pack lifecycle changed before stop; aborting held-secret flight",
                    );
                    return;
                }
                Err(gateway_pack::PromoteCommitError::LifecycleChangedAfterStop) => {
                    // Stop already ran. Concurrent Enable may have seen
                    // had_child==false and skipped governed restart. If the
                    // pack is still enabled, attempt governed with fresh
                    // Keychain secrets rather than pinning Direct forever.
                    //
                    // Re-read enablement NOW — do not use attempt-start
                    // `persisted` (always true to have reached ReadyToPromote).
                    // Disable/Stop/Uninstall during stop/wait must yield
                    // RestoreDirect, not a governed restart against intent.
                    let current_enabled = load_or_create_private_config()
                        .map(|cfg| cfg.via_gateway_default)
                        .unwrap_or(false);
                    let recovery = gateway_pack::promote_after_stop_lifecycle_recovery(
                        current_enabled,
                    );
                    match recovery {
                        gateway_pack::AfterStopLifecycleRecovery::AttemptGovernedFresh => {
                            let _ = app.emit(
                                "council-log",
                                "[system] governed-promote: lifecycle changed after stop; attempting fresh governed start (no held secrets)",
                            );
                            match try_start_council_server(
                                &app,
                                config.server_port,
                                token.as_deref(),
                                Some(true),
                                config.librarian_base.as_deref(),
                            ) {
                                Ok(msg) => {
                                    let _ = app.emit(
                                        "council-log",
                                        format!(
                                            "[system] governed-promote: fresh governed start ok: {msg}"
                                        ),
                                    );
                                    let _ = gateway_pack::status_with_council_route(
                                        &store, true, false,
                                    );
                                    let _ =
                                        status_authority::recompute(&app, Freshness::Action);
                                    return;
                                }
                                Err(ge) => {
                                    let _ = app.emit(
                                        "council-log",
                                        format!(
                                            "[system] governed-promote: fresh governed start failed ({ge}); restoring Direct"
                                        ),
                                    );
                                    match try_start_council_server(
                                        &app,
                                        config.server_port,
                                        token.as_deref(),
                                        Some(false),
                                        config.librarian_base.as_deref(),
                                    ) {
                                        Ok(msg) => {
                                            let _ = app.emit(
                                                "council-log",
                                                format!(
                                                    "[system] governed-promote: Council restored in Direct mode: {msg}"
                                                ),
                                            );
                                        }
                                        Err(re) => {
                                            let _ = app.emit(
                                                "council-log",
                                                format!(
                                                    "[system] governed-promote: Direct restart failed after lifecycle abort: {re}. Core War Room is down; start Council manually."
                                                ),
                                            );
                                        }
                                    }
                                    // Re-schedule without held secrets so recovery can continue.
                                    schedule_governed_promote_attempts(
                                        app.clone(),
                                        auth_token.clone(),
                                        None,
                                        None,
                                    );
                                    let _ =
                                        status_authority::recompute(&app, Freshness::Action);
                                    return;
                                }
                            }
                        }
                        gateway_pack::AfterStopLifecycleRecovery::RestoreDirect => {
                            let _ = app.emit(
                                "council-log",
                                "[system] governed-promote: pack no longer enabled after stop; restoring Direct",
                            );
                            match try_start_council_server(
                                &app,
                                config.server_port,
                                token.as_deref(),
                                Some(false),
                                config.librarian_base.as_deref(),
                            ) {
                                Ok(msg) => {
                                    let _ = app.emit(
                                        "council-log",
                                        format!(
                                            "[system] governed-promote: Council restored in Direct mode: {msg}"
                                        ),
                                    );
                                }
                                Err(re) => {
                                    let _ = app.emit(
                                        "council-log",
                                        format!(
                                            "[system] governed-promote: Direct restart failed after lifecycle abort: {re}. Core War Room is down; start Council manually."
                                        ),
                                    );
                                }
                            }
                            let _ = status_authority::recompute(&app, Freshness::Action);
                            return;
                        }
                    }
                }
                Err(gateway_pack::PromoteCommitError::SpawnFailed(e)) => {
                    let _ = app.emit(
                        "council-log",
                        format!(
                            "[system] governed-promote attempt {}: governed spawn failed ({e}); restoring Direct",
                            attempt + 1
                        ),
                    );
                    match try_start_council_server(
                        &app,
                        config.server_port,
                        token.as_deref(),
                        Some(false),
                        config.librarian_base.as_deref(),
                    ) {
                        Ok(msg) => {
                            let _ = app.emit(
                                "council-log",
                                format!(
                                    "[system] governed-promote: Council restored in Direct mode: {msg}"
                                ),
                            );
                        }
                        Err(re) => {
                            let _ = app.emit(
                                "council-log",
                                format!(
                                    "[system] governed-promote: Direct restart failed after governed spawn failure: {re}. Core War Room is down; start Council manually."
                                ),
                            );
                        }
                    }
                    let _ = status_authority::recompute(&app, Freshness::Action);
                }
            }
        }
        let _ = app.emit(
            "council-log",
            "[system] governed-promote: bounded retries exhausted; pack remains Direct until Enable or next launch",
        );
    });
}

/// Spawn an app-owned `council --serve` child.
///
/// Packaged installs always own the bundled Council. Debug builds may own a
/// repo-built sidecar. An occupied Council port is a startup conflict — this
/// shell never adopts an external process. Gateway remains optional
/// (`via_gateway` default off for packaged installs).
///
/// `via_gateway`: `Some(_)` sets `COUNCIL_VIA_GATEWAY` explicitly ("1"/"0"); `None` inherits
/// (packaged installs force `Some(false)` unless the caller opts in).
fn try_start_council_server(
    app: &AppHandle,
    server_port: Option<u16>,
    auth_token: Option<&str>,
    via_gateway: Option<bool>,
    librarian_base: Option<&str>,
) -> Result<String, String> {
    try_start_council_server_with_credentials(
        app,
        server_port,
        auth_token,
        via_gateway,
        librarian_base,
        None,
    )
}

// ---------------------------------------------------------------------------
// Council start phases — named extraction of try_start_council_server_with_credentials.
// Ordering and authority checks are unchanged; each phase is a pure move of
// the prior sequential block into a domain-named function.
// ---------------------------------------------------------------------------

/// Phase 1 product: port + packaging identity for this start attempt.
struct CouncilStartPlan {
    port: u16,
    packaged: bool,
    expected_sha: &'static str,
    expected_dirty: bool,
}

/// Phase 3 product: binary and writable layout for the owned child.
struct CouncilSpawnLayout {
    effective: String,
    spawn_base_str: String,
    child_cwd: String,
    sessions_dir_env: Option<String>,
}

/// Phase 4 product: Gateway mode + Keychain-sourced child credentials.
struct CouncilGatewayAuthority {
    via_gateway: Option<bool>,
    gateway_creds: Option<GatewayChildCredentials>,
}

/// Phase 1 — resolve start plan (port validation + packaging identity).
fn resolve_start_plan(server_port: Option<u16>) -> Result<CouncilStartPlan, String> {
    let port = match server_port {
        Some(port) => port,
        None => default_serve_port()?,
    };
    validate_serve_port(port)?;
    eprintln!("[council-runtime] start requested on :{port}");

    let packaged = is_packaged_install();
    let (expected_sha, expected_dirty) = bundled_build_identity();
    Ok(CouncilStartPlan {
        port,
        packaged,
        expected_sha,
        expected_dirty,
    })
}

/// Phase 2 — ensure the Council port is free (never adopt an external process).
fn ensure_council_port_available(
    plan: &CouncilStartPlan,
    auth_token: Option<&str>,
) -> Result<(), String> {
    let port = plan.port;
    match probe_council_server(
        port,
        Duration::from_millis(750),
        plan.expected_sha,
        plan.expected_dirty,
        auth_token,
    ) {
        CouncilServerProbe::MatchingBuild => Err(format!(
            "port {port} is already occupied by a Council process; free the port before launching this app (this app will not adopt or kill it)"
        )),
        CouncilServerProbe::DifferentBuild => Err(format!(
            "Council on :{port} has a different source identity; quit the other Council \
             process or free the port before launching this app (this app will not kill it)"
        )),
        CouncilServerProbe::Unavailable => {
            if !wait_for_port_release(port, Duration::from_millis(0)) {
                return Err(format!(
                    "port {port} is occupied by a non-canonical or unhealthy process"
                ));
            }
            Ok(())
        }
    }
}

/// Phase 3 — resolve spawn layout (self-start gate, binary, base-dir, cwd).
fn resolve_spawn_layout(packaged: bool) -> Result<CouncilSpawnLayout, String> {
    // Packaged release owns the bundled sidecar. Debug owns a repo-built sidecar.
    // Unpackaged release (dev shell without bundle) cannot self-start Council.
    if !packaged && !cfg!(debug_assertions) {
        return Err(
            "Council is not running and this build is not a self-contained app bundle. \
             Use the DMG-packaged app, or run `make warroom` for source browser development."
                .to_string(),
        );
    }

    let effective = resolve_council_binary()?;
    let effective = effective.to_string_lossy().into_owned();

    // Packaged: writable Application Support overlay of bundled base-dir (cabinets save).
    // Dev: council-rs repo root.
    let spawn_base = if packaged {
        let bundled = paths::bundled_base_dir().ok_or_else(|| {
            "packaged install is missing Contents/Resources/council-base".to_string()
        })?;
        ensure_writable_base_overlay(&bundled)?
    } else {
        resolve_spawn_base_dir()?
    };
    let spawn_base_str = spawn_base.to_string_lossy().into_owned();

    // Packaged: writable state under Application Support (Resources is read-only).
    // Dev: cwd = council-rs root so relative sessions/ resolve like make warroom.
    let (child_cwd, sessions_dir_env) = if packaged {
        let support = private_config::app_support_dir();
        let sessions = support.join("sessions");
        let _ = std::fs::create_dir_all(&sessions);
        (
            support.to_string_lossy().into_owned(),
            Some(sessions.to_string_lossy().into_owned()),
        )
    } else {
        (
            resolve_council_rs_dir().to_string_lossy().into_owned(),
            None,
        )
    };

    Ok(CouncilSpawnLayout {
        effective,
        spawn_base_str,
        child_cwd,
        sessions_dir_env,
    })
}

/// Phase 4 — resolve Gateway authority (mode default + Keychain credentials).
fn resolve_gateway_authority(
    packaged: bool,
    via_gateway: Option<bool>,
    preloaded_gateway_creds: Option<&GatewayChildCredentials>,
) -> Result<CouncilGatewayAuthority, String> {
    // Packaged installs default Gateway off so missing Docker cannot break core War Room.
    let via_gateway = if packaged {
        Some(via_gateway.unwrap_or(false))
    } else {
        via_gateway
    };

    // Keychain-sourced GW_API_KEY for governed mode only. Never from login shell.
    let gateway_creds: Option<GatewayChildCredentials> = if via_gateway == Some(true) {
        let store = KeychainSecretStore;
        if let Some(credentials) = preloaded_gateway_creds {
            if packaged {
                let st = gateway_pack::gateway_pack_status_fresh_with_key(
                    &store,
                    Some(&credentials.api_key),
                );
                if !st.spawn_capable
                    || !credentials
                        .watch_admin_token
                        .as_deref()
                        .is_some_and(gateway_pack::watch_admin_surfaces_authenticated)
                {
                    return Err(format!(
                        "Gateway models and Watch/Outbox auth are not ready ({}). {}",
                        st.state.as_str(),
                        st.message
                    ));
                }
            }
            Some(credentials.clone())
        } else {
            match load_gw_api_key(&store) {
                Ok(Some(api_key)) => {
                    if packaged {
                        // Thread the already-loaded key: the fresh auth probe runs
                        // /v1/models live without a redundant GW_API_KEY Keychain get.
                        let st = gateway_pack::gateway_pack_status_fresh_with_key(
                            &store,
                            Some(&api_key),
                        );
                        if !st.spawn_capable {
                            return Err(format!(
                                "Gateway is not authenticated-ready ({}). {}",
                                st.state.as_str(),
                                st.message
                            ));
                        }
                    }
                    let watch_admin_token = load_watch_admin_token(&store)
                        .map_err(|e| format!("Watch/Outbox admin Keychain read failed: {e}"))?;
                    if packaged
                        && !watch_admin_token
                            .as_deref()
                            .is_some_and(gateway_pack::watch_admin_surfaces_authenticated)
                    {
                        return Err(
                            "Gateway models auth passed but Watch/Outbox admin auth is not ready"
                                .to_string(),
                        );
                    }
                    Some(GatewayChildCredentials {
                        api_key,
                        gateway_url: docker_cli::DESKTOP_GATEWAY_URL.to_string(),
                        watch_admin_token,
                    })
                }
                Ok(None) => {
                    return Err(
                        "GW_API_KEY is not in the macOS Keychain. Use Settings → Enable Gateway \
                     (installed release) or provision a client key before enabling governed mode."
                            .to_string(),
                    );
                }
                Err(e) => return Err(format!("Keychain read failed: {e}")),
            }
        }
    } else {
        None
    };

    Ok(CouncilGatewayAuthority {
        via_gateway,
        gateway_creds,
    })
}

/// Phase 5 — compose env/args, spawn the owned child, register ownership, notify.
fn spawn_and_register_council(
    app: &AppHandle,
    mut guard: std::sync::MutexGuard<'_, TrackedChild>,
    plan: &CouncilStartPlan,
    layout: CouncilSpawnLayout,
    gateway: CouncilGatewayAuthority,
    auth_token: Option<&str>,
    librarian_base: Option<&str>,
) -> Result<String, String> {
    let CouncilStartPlan { port, packaged, .. } = *plan;
    let CouncilSpawnLayout {
        effective,
        spawn_base_str,
        child_cwd,
        sessions_dir_env,
    } = layout;
    let CouncilGatewayAuthority {
        via_gateway,
        gateway_creds,
    } = gateway;

    let cors_origins = build_cors_origins(port);
    // Packaged: pass bundled War Room export via --web-dist when present so the
    // phone surface shares Council :8765 (no permanent :3010). Dev omits it.
    let web_dist = if packaged {
        paths::bundled_web_dist().map(|p| p.to_string_lossy().into_owned())
    } else {
        None
    };
    let args = compose_sidecar_args(&spawn_base_str, port, web_dist.as_deref());

    let mut command = app
        .shell()
        .command(&effective)
        .current_dir(&child_cwd)
        .args(args);
    for (key, value) in compose_sidecar_env(
        cors_origins.as_str(),
        // Packaged release must not use COUNCIL_DEV_NO_AUTH; debug may.
        cfg!(debug_assertions) && !packaged,
        auth_token,
        via_gateway,
        librarian_base,
        gateway_creds.as_ref(),
    ) {
        command = command.env(key, value);
    }
    if let Some(sessions_dir) = sessions_dir_env {
        command = command.env("COUNCIL_SESSIONS_DIR", sessions_dir);
    }
    // Finder/GUI launch: inject login PATH + provider keys so Discover works without a terminal.
    // Never imports GW_API_KEY (filtered in is_council_provider_env_key).
    if packaged {
        for (key, value) in gui_login_environment() {
            command = command.env(key, value);
        }
        // Re-apply compose env after login merge so Gateway scrub/inject is authoritative.
        for (key, value) in compose_sidecar_env(
            cors_origins.as_str(),
            cfg!(debug_assertions) && !packaged,
            auth_token,
            via_gateway,
            librarian_base,
            gateway_creds.as_ref(),
        ) {
            command = command.env(key, value);
        }
        let _ = app.emit(
            "council-log",
            "[system] packaged spawn: login PATH/provider env merged for GUI launch (values not logged)",
        );
    }
    if via_gateway == Some(true) {
        let _ = app.emit(
            "council-log",
            "[system] Gateway mode: COUNCIL_VIA_GATEWAY=1 with Keychain-sourced GW_API_KEY (value not logged)",
        );
    }

    let (mut rx, child) = command
        .spawn()
        .map_err(|e| format!("failed to spawn council: {e}"))?;

    // Tie this spawn's log pump to a generation so its Terminated cleanup
    // cannot clear a child respawned later (restart race, see TrackedChild).
    guard.generation = guard.generation.wrapping_add(1);
    let spawn_generation = guard.generation;

    let app_for_logs = app.clone();

    {
        let logs_state = app.state::<ServerLogs>();
        if let Ok(mut log_guard) = logs_state.0.lock() {
            log_guard.clear();
        };
    }
    let _ = app.emit("council-log", "[system] council server starting...");

    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            let mut terminated = false;
            let line = match event {
                CommandEvent::Stdout(data) => {
                    let s = String::from_utf8_lossy(&data).trim().to_string();
                    if s.is_empty() {
                        continue;
                    }
                    format!("[stdout] {}", s)
                }
                CommandEvent::Stderr(data) => {
                    let s = String::from_utf8_lossy(&data).trim().to_string();
                    if s.is_empty() {
                        continue;
                    }
                    format!("[stderr] {}", s)
                }
                CommandEvent::Error(e) => format!("[shell-error] {}", e),
                CommandEvent::Terminated(t) => {
                    terminated = true;
                    format!("[terminated] code={:?} signal={:?}", t.code, t.signal)
                }
                _ => continue,
            };

            let _ = app_for_logs.emit("council-log", &line);

            {
                let logs_state = app_for_logs.state::<ServerLogs>();
                if let Ok(mut log_guard) = logs_state.0.lock() {
                    log_guard.push(line);
                    if log_guard.len() > 500 {
                        let drain = log_guard.len() - 500;
                        log_guard.drain(0..drain);
                    }
                };
            }

            if terminated {
                let server_state = app_for_logs.state::<CouncilServer>();
                if let Ok(mut server_guard) = server_state.0.lock() {
                    if server_guard.generation == spawn_generation {
                        server_guard.child = None;
                        record_owned_council_pid(None);
                        gateway_pack::record_owned_council_route(None);
                    }
                };
            }
        }
    });

    let owned_pid = child.pid();
    guard.child = Some(child);
    record_owned_council_pid(Some(owned_pid));
    gateway_pack::record_owned_council_route(Some(via_gateway == Some(true)));
    drop(guard);

    // Cache the spawn config so restart_sidecar can respawn with the same
    // pairing token (token is not re-sent by the frontend).
    {
        let config_state = app.state::<SpawnConfigCache>();
        if let Ok(mut config_guard) = config_state.0.lock() {
            *config_guard = LastSpawnConfig {
                server_port: Some(port),
                auth_token: auth_token.map(str::to_string),
                librarian_base: librarian_base.map(str::to_string),
            };
        };
    }

    // Ownership proof line for packaged/native smokes (stderr → app.log).
    eprintln!("council --serve started on :{port}");

    let _ = app
        .notification()
        .builder()
        .title("IRIN")
        .body(format!("Sidecar council --serve started on :{port}"))
        .show();

    Ok(format!(
        "council --serve started on :{port} (bin: {effective}, base-dir: {spawn_base_str}). \
         WS/REST should be reachable from Tauri webview"
    ))
}

/// Spawn an app-owned `council --serve` child.
///
/// Implemented as five named phases (start plan → port availability → spawn
/// layout → Gateway authority → spawn/register). Inputs, outputs, ordering,
/// and authority checks are unchanged from the pre-split monolithic start path.
fn try_start_council_server_with_credentials(
    app: &AppHandle,
    server_port: Option<u16>,
    auth_token: Option<&str>,
    via_gateway: Option<bool>,
    librarian_base: Option<&str>,
    preloaded_gateway_creds: Option<&GatewayChildCredentials>,
) -> Result<String, String> {
    let state = app.state::<CouncilServer>();
    let guard = state.0.lock().map_err(|e| e.to_string())?;
    if guard.child.is_some() {
        return Ok("council server already tracked as running".to_string());
    }

    let plan = resolve_start_plan(server_port)?;
    ensure_council_port_available(&plan, auth_token)?;
    let layout = resolve_spawn_layout(plan.packaged)?;
    let gateway = resolve_gateway_authority(plan.packaged, via_gateway, preloaded_gateway_creds)?;
    spawn_and_register_council(
        app,
        guard,
        &plan,
        layout,
        gateway,
        auth_token,
        librarian_base,
    )
}

/// Start an app-owned Council for debug/source desktop shells.
/// Packaged installs spawn the bundled sidecar from native setup; frontend
/// start is refused so it cannot race the governed restore path.
/// Sets `COUNCIL_CORS_ORIGINS` for Tauri asset origins and Next dev (3010) / API port.
/// `COUNCIL_DEV_NO_AUTH` is set only in debug builds; release requires `COUNCIL_AUTH_TOKEN`.
/// The default port is selected at build time from `IRIN_COUNCIL_PORT` in
/// isolated worktrees and remains 8765 for the canonical installed runtime.
#[tauri::command]
async fn start_council_server(
    app: AppHandle,
    server_port: Option<u16>,
    auth_token: Option<String>,
    librarian_base: Option<String>,
) -> Result<String, String> {
    // Packaged / installed-release: native setup owns Council startup. A
    // frontend call with via_gateway=None would force Direct and race the
    // governed restore path ("already tracked as running" without correcting
    // ownership). Refuse the spawn; health polling remains the frontend job.
    if is_packaged_install() {
        return Ok(
            "packaged install: Council startup owned by native setup; frontend start skipped"
                .to_string(),
        );
    }
    try_start_council_server(
        &app,
        server_port,
        auth_token.as_deref(),
        None,
        librarian_base.as_deref(),
    )
}

/// Stop the tracked council server (best effort kill).
#[tauri::command]
async fn stop_council_server(
    app: AppHandle,
    state: State<'_, CouncilServer>,
) -> Result<String, String> {
    let had = state.0.lock().map_err(|e| e.to_string())?.child.is_some();
    stop_tracked_council_server(&app);
    if had {
        Ok("council server stop signal sent".to_string())
    } else {
        Ok("no tracked council server to stop".to_string())
    }
}

/// Restart the council sidecar with gateway routing toggled.
/// Kills the tracked child (if any), waits for its configured port to free up, and respawns
/// with `COUNCIL_VIA_GATEWAY=1` when `via_gateway` is true ("0" when false —
/// explicit off, since the child inherits the parent env). Reuses the cached
/// spawn config so the pairing token survives the restart; if no sidecar is
/// tracked this simply starts one. Returns the same shape as start_council_server.
///
/// Packaged installs refuse `via_gateway=true` unless the Gateway Pack is
/// spawn-capable (enabled + live-authenticated). Restart is the transition
/// that creates the governed-child proof, so it must not demand
/// `governed_ready` (chicken-and-egg). Enroll/arm still require fresh
/// `governed_ready` after the restart completes.
#[tauri::command]
async fn restart_sidecar(
    app: AppHandle,
    via_gateway: bool,
    librarian_base: Option<String>,
) -> Result<String, String> {
    // Port-release polling blocks (up to 5s) — keep it off the async runtime.
    tauri::async_runtime::spawn_blocking(move || {
        if via_gateway && is_packaged_install() {
            let store = KeychainSecretStore;
            // Authority path: fresh sample; gate is spawn_capable not governed_ready.
            let st = gateway_pack::gateway_pack_status_fresh(&store);
            if !st.spawn_capable {
                return Err(format!(
                    "Cannot enable governed mode: Gateway Pack is {} — {}. \
                     Use Settings → Enable Gateway first.",
                    st.state.as_str(),
                    st.message
                ));
            }
        }

        let config = {
            let state = app.state::<SpawnConfigCache>();
            let guard = state.0.lock().map_err(|e| e.to_string())?;
            guard.clone()
        };

        let had_child = {
            let state = app.state::<CouncilServer>();
            let guard = state.0.lock().map_err(|e| e.to_string())?;
            guard.child.is_some()
        };
        let port = match config.server_port {
            Some(port) => port,
            None => default_serve_port()?,
        };
        if !had_child {
            let (expected_sha, expected_dirty) = bundled_build_identity();
            match probe_council_server(
                port,
                Duration::from_millis(750),
                expected_sha,
                expected_dirty,
                config.auth_token.as_deref(),
            ) {
                CouncilServerProbe::MatchingBuild | CouncilServerProbe::DifferentBuild => {
                    return Err(format!(
                        "port {port} is occupied by another Council process; free the port before restarting (this app only restarts the Council child it owns)"
                    ));
                }
                CouncilServerProbe::Unavailable => {
                    if !wait_for_port_release(port, Duration::from_millis(0)) {
                        return Err(
                            format!("port {port} is occupied by a non-canonical or unhealthy process"),
                        );
                    }
                }
            }
        }
        stop_tracked_council_server(&app);
        if had_child {
            // kill() returns before the OS releases the listener; wait so the
            // respawn does not lose the bind race on the configured port.
            if !wait_for_port_release(port, Duration::from_secs(5)) {
                let _ = app.emit(
                    "council-log",
                    format!(
                        "[system] restart: port {port} still busy after 5s; spawning anyway"
                    ),
                );
            }
        }

        try_start_council_server(
            &app,
            Some(port),
            config.auth_token.as_deref(),
            Some(via_gateway),
            librarian_base.as_deref().or(config.librarian_base.as_deref()),
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Non-secret Gateway Pack status for the installed-release UI.
#[tauri::command]
async fn gateway_pack_status() -> Result<GatewayPackStatus, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let store = KeychainSecretStore;
        Ok(gateway_pack::gateway_pack_status(&store))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Install/start/provision/enable the app-owned Gateway Pack. Never returns secrets.
/// Ready only when Gateway auth **and** owned Council governed restart both succeed.
/// Returns a committed host-authoritative status snapshot after the mutation.
#[tauri::command]
async fn gateway_pack_enable(app: AppHandle) -> Result<DesktopStatusSnapshot, String> {
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let store = KeychainSecretStore;
        let status = gateway_pack::enable_gateway_pack(&store)?;
        // Docker missing/down is neutral for core Direct — still recompute.
        if matches!(
            status.state,
            GatewayPackState::DockerMissing | GatewayPackState::DockerDaemonDown
        ) {
            return Ok(status_authority::recompute(&app2, Freshness::Action));
        }
        if !status.authenticated || !status.enabled {
            return Ok(status_authority::recompute(&app2, Freshness::Action));
        }

        // Restart owned Council child into governed mode with Keychain key.
        let config = {
            let state = app2.state::<SpawnConfigCache>();
            let guard = state.0.lock().map_err(|e| e.to_string())?;
            guard.clone()
        };
        let had_child = {
            let state = app2.state::<CouncilServer>();
            let guard = state.0.lock().map_err(|e| e.to_string())?;
            guard.child.is_some()
        };
        if !had_child {
            // No owned child: pack auth alone is not full ready for this shell.
            let _ = gateway_pack::status_with_council_route(&store, false, false);
            return Ok(status_authority::recompute(&app2, Freshness::Action));
        }
        stop_tracked_council_server(&app2);
        let _ = wait_for_port_release(default_serve_port().unwrap_or(8765), Duration::from_secs(5));
        match try_start_council_server(
            &app2,
            None,
            config.auth_token.as_deref(),
            Some(true),
            config.librarian_base.as_deref(),
        ) {
            Ok(msg) => {
                let _ = app2.emit("council-log", format!("[system] gateway enable: {msg}"));
                let _ = gateway_pack::status_with_council_route(&store, true, false);
                Ok(status_authority::recompute(&app2, Freshness::Action))
            }
            Err(e) => {
                let _ = app2.emit(
                    "council-log",
                    format!("[system] gateway enable: council governed restart failed: {e}"),
                );
                // Roll back: via_gateway_default=true was persisted and the
                // working Direct child was already stopped. Restore the
                // persisted route to Direct, then try to bring core War Room
                // back up before returning the enable error — never leave the
                // app down with state claiming governed.
                if let Err(disable_err) = gateway_pack::disable_gateway_pack(&store) {
                    let _ = app2.emit(
                        "council-log",
                        format!("[system] gateway enable rollback: failed to restore Direct config: {disable_err}"),
                    );
                }
                let _ = wait_for_port_release(
                    default_serve_port().unwrap_or(8765),
                    Duration::from_secs(5),
                );
                let rollback_note = match try_start_council_server(
                    &app2,
                    None,
                    config.auth_token.as_deref(),
                    Some(false),
                    config.librarian_base.as_deref(),
                ) {
                    Ok(msg) => {
                        let _ = app2.emit(
                            "council-log",
                            format!("[system] gateway enable rollback: Council restored in Direct mode: {msg}"),
                        );
                        "Council was restored in Direct mode.".to_string()
                    }
                    Err(re) => {
                        let _ = app2.emit(
                            "council-log",
                            format!("[system] gateway enable rollback: Direct restart failed: {re}"),
                        );
                        format!(
                            "Direct-mode rollback restart also failed: {re}. \
                             Core War Room is down; start Council manually."
                        )
                    }
                };
                // Propagate failure — do not claim authenticated-ready.
                // Still commit a truthful post-failure snapshot for the panel.
                let _ = status_authority::recompute(&app2, Freshness::Action);
                Err(format!(
                    "Gateway pack authenticated but Council governed restart failed: {e}. \
                     Rolled back to Direct (via_gateway_default=false). {rollback_note}"
                ))
            }
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Disable governed mode and restart Council in Direct mode. Keeps pack data/Keychain.
/// Propagates Council Direct restart failures.
#[tauri::command]
async fn gateway_pack_disable(app: AppHandle) -> Result<DesktopStatusSnapshot, String> {
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let store = KeychainSecretStore;
        let _status = gateway_pack::disable_gateway_pack(&store)?;
        let config = {
            let state = app2.state::<SpawnConfigCache>();
            let guard = state.0.lock().map_err(|e| e.to_string())?;
            guard.clone()
        };
        let had_child = {
            let state = app2.state::<CouncilServer>();
            let guard = state.0.lock().map_err(|e| e.to_string())?;
            guard.child.is_some()
        };
        if had_child {
            stop_tracked_council_server(&app2);
            let _ =
                wait_for_port_release(default_serve_port().unwrap_or(8765), Duration::from_secs(5));
            try_start_council_server(
                &app2,
                None,
                config.auth_token.as_deref(),
                Some(false),
                config.librarian_base.as_deref(),
            )
            .map_err(|e| format!("Gateway disabled but Council Direct restart failed: {e}"))?;
            let _ = app2.emit(
                "council-log",
                "[system] gateway disable: Council restarted in Direct mode",
            );
            let _ = gateway_pack::status_with_council_route(&store, false, true);
        } else {
            let _ = gateway_pack::status_with_council_route(&store, false, true);
        }
        Ok(status_authority::recompute(&app2, Freshness::Action))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Stop the desktop Compose project only (no volume delete).
/// Switches to Direct first (via stop_gateway_pack) and restarts owned Council in Direct.
#[tauri::command]
async fn gateway_pack_stop(app: AppHandle) -> Result<DesktopStatusSnapshot, String> {
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let store = KeychainSecretStore;
        // Ensure Direct config before containers stop.
        let status = gateway_pack::stop_gateway_pack(&store)?;
        let config = {
            let state = app2.state::<SpawnConfigCache>();
            let guard = state.0.lock().map_err(|e| e.to_string())?;
            guard.clone()
        };
        let had_child = {
            let state = app2.state::<CouncilServer>();
            let guard = state.0.lock().map_err(|e| e.to_string())?;
            guard.child.is_some()
        };
        if had_child {
            stop_tracked_council_server(&app2);
            let _ =
                wait_for_port_release(default_serve_port().unwrap_or(8765), Duration::from_secs(5));
            try_start_council_server(
                &app2,
                None,
                config.auth_token.as_deref(),
                Some(false),
                config.librarian_base.as_deref(),
            )
            .map_err(|e| {
                gateway_pack::lifecycle_stage("stop_handler_complete", "error");
                format!("Gateway pack stopped but Council Direct restart failed: {e}")
            })?;
        }
        let mut st = gateway_pack::status_with_council_route(&store, false, true);
        if status.docker == "ready" {
            st.message = "Gateway pack stopped; Council is in Direct mode.".into();
        }
        gateway_pack::lifecycle_stage("stop_handler_complete", "ok");
        let _ = st;
        Ok(status_authority::recompute(&app2, Freshness::Action))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Install or remove the pack-native watch profile and recreate the pack so the
/// sidecar reloads. Toggle is a bounded force-recreate (in-flight requests drop).
#[tauri::command]
async fn gateway_pack_set_watch_sentinels(enabled: bool) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let store = KeychainSecretStore;
        gateway_pack::set_watch_sentinels_enabled(&store, enabled)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Whether the durable watch profile file is installed under app-support.
#[tauri::command]
fn gateway_pack_watch_sentinels_enabled() -> bool {
    gateway_pack::watch_sentinels_enabled()
}

/// Absolute host path of the watch inbox folder (created if needed).
#[tauri::command]
fn gateway_pack_watch_inbox_path() -> Result<String, String> {
    gateway_pack::watch_inbox_path_string()
}

/// Reveal the watch inbox in Finder (macOS).
#[tauri::command]
fn gateway_pack_open_watch_inbox() -> Result<String, String> {
    gateway_pack::open_watch_inbox()
}

/// Destructive uninstall of the desktop pack only. Explicit operator action.
/// Propagates Council Direct restart failures.
#[tauri::command]
async fn gateway_pack_uninstall(app: AppHandle) -> Result<DesktopStatusSnapshot, String> {
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let store = KeychainSecretStore;
        let _status = gateway_pack::uninstall_gateway_pack(&store)?;
        let config = {
            let state = app2.state::<SpawnConfigCache>();
            let guard = state.0.lock().map_err(|e| e.to_string())?;
            guard.clone()
        };
        let had_child = {
            let state = app2.state::<CouncilServer>();
            let guard = state.0.lock().map_err(|e| e.to_string())?;
            guard.child.is_some()
        };
        if had_child {
            stop_tracked_council_server(&app2);
            let _ =
                wait_for_port_release(default_serve_port().unwrap_or(8765), Duration::from_secs(5));
            try_start_council_server(
                &app2,
                None,
                config.auth_token.as_deref(),
                Some(false),
                config.librarian_base.as_deref(),
            )
            .map_err(|e| {
                format!("Gateway pack uninstalled but Council Direct restart failed: {e}")
            })?;
        }
        Ok(status_authority::recompute(&app2, Freshness::Action))
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------------------
// Touch ID product control
//
// The renderer can only trigger these fixed workflows and read the non-secret
// status projection. Helper invocation, the Secure Enclave signature, the
// arm-principal bearer, and stage/confirm/disarm stay native. Nothing runs at
// launch.
// ---------------------------------------------------------------------------

/// Fresh pack readiness for enroll/arm ceremonies — never uses presentation
/// sticky. Presentation readiness lives in `status_authority` (single sticky).
fn gateway_ready_for_arm() -> bool {
    let store = KeychainSecretStore;
    gateway_pack::gateway_pack_status_fresh(&store).governed_ready
}

#[tauri::command]
async fn touch_id_status(app: AppHandle) -> Result<touch_id::TouchIdStatus, String> {
    // Presentation path: consume status_authority snapshot (single sticky),
    // not a parallel GATEWAY_READY sticky recomputed here.
    tauri::async_runtime::spawn_blocking(move || {
        let snap = status_authority::recompute(&app, Freshness::Background);
        Ok(snap.touch_id)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn touch_id_enroll(app: AppHandle) -> Result<DesktopStatusSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let store = KeychainSecretStore;
        touch_id::enroll(&store, gateway_ready_for_arm())?;

        // Enrollment changes both the public credential registry and the
        // Keychain-held arm principal. Refresh the operator-owned pack now so
        // its boot-time allowlists match; setup is explicit, but no producer is
        // armed by this action.
        let pack = gateway_pack::enable_gateway_pack(&store)?;
        // Post-lifecycle check: enable returns a fresh sample; require pack auth
        // presentation (enable sets AuthenticatedReady before Council restart).
        if !pack.enabled || !pack.authenticated || !pack.spawn_capable {
            let _ = status_authority::recompute(&app, Freshness::Action);
            return Err(
                "Touch ID enrolled, but Gateway could not reload its arm registry".to_string(),
            );
        }
        Ok(status_authority::recompute(&app, Freshness::Action))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn touch_id_arm(app: AppHandle) -> Result<DesktopStatusSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let store = KeychainSecretStore;
        // Fail closed on a fresh sample — never arm from sticky/cached presentation.
        if !gateway_ready_for_arm() {
            return Err(
                "Gateway must be enabled and authenticated before Touch ID arm".to_string(),
            );
        }
        touch_id::arm(&store)?;
        Ok(status_authority::recompute(&app, Freshness::Action))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn touch_id_renew(app: AppHandle) -> Result<DesktopStatusSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let store = KeychainSecretStore;
        // Fail closed on a fresh sample — renew is the same ceremony as arm.
        if !gateway_ready_for_arm() {
            return Err(
                "Gateway must be enabled and authenticated before Touch ID renew".to_string(),
            );
        }
        touch_id::renew(&store)?;
        Ok(status_authority::recompute(&app, Freshness::Action))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn touch_id_disarm(app: AppHandle) -> Result<DesktopStatusSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let store = KeychainSecretStore;
        touch_id::disarm(&store)?;
        Ok(status_authority::recompute(&app, Freshness::Action))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Host-authoritative combined status snapshot (Background freshness).
#[tauri::command]
async fn desktop_status_snapshot(app: AppHandle) -> Result<DesktopStatusSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        Ok(status_authority::recompute(&app, Freshness::Background))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Native file picker (cabinet yamls, session json, map dirs, etc.).
#[tauri::command]
async fn pick_file(app: AppHandle) -> Result<Option<String>, String> {
    let picked = app.dialog().file().blocking_pick_file();
    Ok(picked.map(|p| p.to_string()))
}

/// Simple health check helper (frontend can also fetch /api/health directly).
#[tauri::command]
async fn ping_council() -> Result<String, String> {
    Ok("ok - use /api/health from UI for real council status".to_string())
}

/// Return the current captured backend logs (for the "Backend" tab).
#[tauri::command]
async fn get_server_logs(state: State<'_, ServerLogs>) -> Result<Vec<String>, String> {
    let logs = state.0.lock().map_err(|e| e.to_string())?;
    Ok(logs.clone())
}

/// Clear the backend log buffer.
#[tauri::command]
async fn clear_server_logs(state: State<'_, ServerLogs>) -> Result<(), String> {
    let mut logs = state.0.lock().map_err(|e| e.to_string())?;
    logs.clear();
    Ok(())
}

/// Native save for synthesis text.
#[tauri::command]
async fn save_synthesis(app: AppHandle, text: String) -> Result<String, String> {
    let path = app
        .dialog()
        .file()
        .set_file_name("synthesis.md")
        .blocking_save_file();
    if let Some(p) = path {
        let pstr = p.to_string();
        std::fs::write(&pstr, text).map_err(|e| e.to_string())?;
        Ok(format!("Saved to {}", pstr))
    } else {
        Ok("Save cancelled".to_string())
    }
}

/// Native save for PDF bytes using OS dialog (for gate #10f).
#[tauri::command]
async fn save_pdf(app: AppHandle, data: Vec<u8>, filename: String) -> Result<String, String> {
    let path = app
        .dialog()
        .file()
        .set_file_name(&filename)
        .blocking_save_file();
    if let Some(p) = path {
        let pstr = p.to_string();
        std::fs::write(&pstr, data).map_err(|e| e.to_string())?;
        Ok(format!("Saved to {}", pstr))
    } else {
        Ok("Save cancelled".to_string())
    }
}

/// Whether Gateway Pack is currently enabled (presentation/status sample).
fn gateway_pack_enabled_flag() -> bool {
    let store = KeychainSecretStore;
    gateway_pack::gateway_pack_status(&store).enabled
}

/// Fresh enabled flag for authority-bearing phone publication changes.
fn gateway_pack_enabled_flag_fresh() -> bool {
    let store = KeychainSecretStore;
    gateway_pack::gateway_pack_status_fresh(&store).enabled
}

/// Authenticated, build-matched readiness of the **app-owned** Council child.
/// This is the only readiness proof accepted before publishing Council to the
/// operator's tailnet. An external process on the port never counts.
fn council_backend_ready(app: &AppHandle) -> bool {
    council_backend_ready_probe(app)
}

/// Shared probe used by phone publication and the status authority.
fn council_backend_ready_probe(app: &AppHandle) -> bool {
    let owned = {
        let state = app.state::<CouncilServer>();
        state.0.lock().map(|g| g.child.is_some()).unwrap_or(false)
    };
    if !owned {
        return false;
    }
    let auth_token = {
        let state = app.state::<SpawnConfigCache>();
        state
            .0
            .lock()
            .ok()
            .and_then(|config| config.auth_token.clone())
    };
    let port = default_serve_port().unwrap_or(8765);
    let (expected_sha, expected_dirty) = bundled_build_identity();
    matches!(
        probe_council_server(
            port,
            Duration::from_millis(400),
            expected_sha,
            expected_dirty,
            auth_token.as_deref(),
        ),
        CouncilServerProbe::MatchingBuild
    )
}

/// Non-secret phone access status (no bearer token, no pairing secret).
#[tauri::command]
async fn phone_access_status(app: AppHandle) -> Result<PhoneAccessStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let gw = gateway_pack_enabled_flag();
        let council_ready = council_backend_ready(&app);
        Ok(phone_access::phone_access_status(
            &LiveTailscaleRunner,
            gw,
            council_ready,
        ))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Enable private phone publication via Tailscale Serve (never Funnel).
#[tauri::command]
async fn phone_access_enable(app: AppHandle) -> Result<DesktopStatusSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // Authority path: publication changes the Tailscale route table.
        let gw = gateway_pack_enabled_flag_fresh();
        let council_ready = council_backend_ready(&app);
        if !council_ready {
            return Err(
                "Council is not authenticated-ready on the bundled build; phone access was not changed"
                    .to_string(),
            );
        }
        phone_access::phone_access_enable(&LiveTailscaleRunner, gw, council_ready)?;
        Ok(status_authority::recompute(&app, Freshness::Action))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Disable phone publication by restoring the prior Serve snapshot.
#[tauri::command]
async fn phone_access_disable(app: AppHandle) -> Result<DesktopStatusSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // Authority path: restores Serve; use a fresh enabled flag.
        let gw = gateway_pack_enabled_flag_fresh();
        let council_ready = council_backend_ready(&app);
        phone_access::phone_access_disable(&LiveTailscaleRunner, gw, council_ready)?;
        Ok(status_authority::recompute(&app, Freshness::Action))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Aggregate product lifecycle: Council, optional Gateway, phone access.
///
/// Pure composition over existing subsystem owners — does not spawn a second
/// Council or Gateway launcher. Quit leaves app-owned Serve configured.
#[tauri::command]
async fn app_lifecycle_status(app: AppHandle) -> Result<AppLifecycleStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let owned_child = {
            let state = app.state::<CouncilServer>();
            state.0.lock().map(|g| g.child.is_some()).unwrap_or(false)
        };
        let health_ready = council_backend_ready(&app);
        let council = classify_council_lifecycle(CouncilLifecycleInput {
            owned_child,
            stopping: false,
            health_ready,
            last_error: false,
        });

        let store = KeychainSecretStore;
        let pack = gateway_pack::gateway_pack_status(&store);
        let gateway = classify_gateway_lifecycle(pack.state);

        let phone =
            phone_access::phone_access_status(&LiveTailscaleRunner, pack.enabled, health_ready);
        let phone_life = classify_phone_lifecycle(phone.state);

        Ok(compose_app_lifecycle(council, gateway, phone_life))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(CouncilServer(Mutex::new(TrackedChild::default())))
        .manage(SpawnConfigCache(Mutex::new(LastSpawnConfig::default())))
        .manage(ServerLogs(Mutex::new(Vec::new())))
        .invoke_handler(tauri::generate_handler![
            start_council_server,
            stop_council_server,
            restart_sidecar,
            gateway_pack_status,
            gateway_pack_enable,
            gateway_pack_disable,
            gateway_pack_stop,
            gateway_pack_uninstall,
            gateway_pack_set_watch_sentinels,
            gateway_pack_watch_sentinels_enabled,
            gateway_pack_watch_inbox_path,
            gateway_pack_open_watch_inbox,
            touch_id_status,
            touch_id_enroll,
            touch_id_arm,
            touch_id_renew,
            touch_id_disarm,
            desktop_status_snapshot,
            phone_access_status,
            phone_access_enable,
            phone_access_disable,
            app_lifecycle_status,
            pick_file,
            ping_council,
            get_server_logs,
            clear_server_logs,
            save_synthesis,
            save_pdf,
            desktop_runtime_mode,
            desktop_runtime_config,
            native_owns_council_startup,
            report_council_runtime_ready
        ])
        .on_page_load(|webview, payload| {
            if !should_reveal_main_window(webview.label(), payload.event()) {
                return;
            }
            let window = webview.window();
            match window.is_visible() {
                Ok(false) => {
                    if let Err(error) = window.show() {
                        eprintln!("[webview] failed to reveal loaded War Room window: {error}");
                    } else {
                        eprintln!("[webview] loaded War Room window revealed");
                        if let Err(error) = window.set_focus() {
                            eprintln!("[webview] failed to focus loaded War Room window: {error}");
                        }
                    }
                }
                Ok(true) => {}
                Err(error) => {
                    eprintln!("[webview] failed to inspect War Room window visibility: {error}")
                }
            }
        })
        .setup(|app| {
            // Fail-closed: if the host process exits without RunEvent::Exit,
            // still reclaim any recorded owned Council PID.
            unsafe {
                let _ = libc::atexit(atexit_kill_owned_council);
            }
            let handle = app.handle().clone();
            let menu = Menu::with_items(
                app,
                &[
                    &MenuItem::with_id(app, "show", "Open War Room", true, None::<&str>)?,
                    &MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?,
                ],
            )?;
            let mut tray_builder = TrayIconBuilder::new()
                .tooltip("IRIN")
                .menu(&menu);
            if let Some(icon) = app.default_window_icon() {
                tray_builder = tray_builder.icon(icon.clone());
            }
            let _tray = tray_builder
                .on_menu_event(move |app_handle, event| match event.id().as_ref() {
                    "show" => {
                        show_main_window(app_handle);
                    }
                    "quit" => {
                        stop_tracked_council_server(app_handle);
                        app_handle.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(move |_tray, event| {
                    if let TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        ..
                    } = event
                    {
                        show_main_window(&handle);
                    }
                })
                .build(app)?;

            // Packaged release: first-launch private config + auto-start bundled Council.
            // Debug: auto-start with COUNCIL_DEV_NO_AUTH via compose_sidecar_env.
            // Unpackaged release: webview may still call start_council_server after configReady.
            {
                let auto_start_handle = app.handle().clone();
                let packaged = is_packaged_install();
                tauri::async_runtime::spawn(async move {
                    // One cold-launch Keychain flight. Legacy migration returns
                    // the GW/pepper values it already read; the remaining four
                    // accounts are loaded once here and threaded through resume
                    // and governed Council spawn.
                    let (launch_key, launch_secrets, preload_auth_generation) = if packaged {
                        let store = KeychainSecretStore;
                        let _preload_flight = keychain::begin_cold_launch_preload();
                        let migrated = migrate_legacy_secrets_with_values(&store);
                        gateway_pack::invalidate_auth_observation();
                        // Capture generation with the flight so a concurrent
                        // Enable cannot commit this key under a newer gen.
                        let preload_auth_generation =
                            gateway_pack::auth_observation_generation();
                        match gateway_pack::load_launch_secrets(&store, migrated.auth_pepper) {
                            Ok(secrets) => {
                                seed_arm_principal_observation(
                                    secrets.arm_principal_token.is_some(),
                                );
                                // Seed presentation auth cache immediately so a
                                // concurrent UI desktop_status_snapshot cannot
                                // re-get GW_API_KEY while resume/spawn runs.
                                // Re-seeded again after pack work for live auth.
                                gateway_pack::seed_auth_observation_from_preloaded_key(
                                    migrated.gw_api_key.as_deref(),
                                    preload_auth_generation,
                                );
                                (migrated.gw_api_key, Some(secrets), preload_auth_generation)
                            }
                            Err(error) => {
                                eprintln!(
                                    "[council-runtime] cold-launch secret preload failed: {error}"
                                );
                                gateway_pack::seed_auth_observation_from_preloaded_key(
                                    migrated.gw_api_key.as_deref(),
                                    preload_auth_generation,
                                );
                                (migrated.gw_api_key, None, preload_auth_generation)
                            }
                        }
                    } else {
                        (None, None, gateway_pack::auth_observation_generation())
                    };
                    let mut persisted_via_gateway = false;
                    let auth_token = if packaged {
                        match load_or_create_private_config() {
                            Ok(cfg) => {
                                persisted_via_gateway = cfg.via_gateway_default;
                                let _ = auto_start_handle.emit(
                                    "council-log",
                                    format!(
                                        "[system] private config ready (install_id={}, via_gateway_default={})",
                                        cfg.install_id, cfg.via_gateway_default
                                    ),
                                );
                                let t = cfg.auth_token.trim().to_string();
                                if t.is_empty() {
                                    None
                                } else {
                                    Some(t)
                                }
                            }
                            Err(e) => {
                                let _ = auto_start_handle.emit(
                                    "council-log",
                                    format!("[system] private config: {e}"),
                                );
                                None
                            }
                        }
                    } else {
                        None
                    };

                    // Debug always auto-starts; packaged release auto-starts bundled Council.
                    if packaged || cfg!(debug_assertions) {
                        let token_ref = auth_token.as_deref();
                        // Packaged launch restores the persisted governed route
                        // after revalidating pack authentication — and when the
                        // pack is not immediately ready, a bounded resume
                        // (compose up + wait) is attempted before fail-closed
                        // Direct. via_gateway_default stays true so a later
                        // promote can succeed without manual re-enable.
                        let mut launch_via_gateway = false;
                        let preloaded_gateway_creds = match (
                            launch_key.as_ref(),
                            launch_secrets.as_ref(),
                        ) {
                            (Some(api_key), Some(secrets)) => Some(GatewayChildCredentials {
                                api_key: api_key.clone(),
                                gateway_url: docker_cli::DESKTOP_GATEWAY_URL.to_string(),
                                watch_admin_token: Some(secrets.watch_admin_token.clone()),
                            }),
                            _ => None,
                        };
                        if packaged && persisted_via_gateway {
                            let store = KeychainSecretStore;
                            let preloaded = launch_key
                                .as_ref()
                                .zip(launch_secrets.as_ref());
                            if preloaded
                                .is_some_and(|(key, _)| gateway_pack::pack_auth_revalidated_with_key(key))
                            {
                                // Pack containers already healthy; still bring up
                                // app-owned host adapters and reconcile the
                                // Watch-token contract before governed spawn.
                                let reconciliation = match preloaded {
                                    Some((key, secrets)) => {
                                        gateway_pack::resume_installed_pack_with_key(
                                            &store, key, secrets,
                                        )
                                    }
                                    None => {
                                        Err("cold-launch Keychain preload unavailable".to_string())
                                    }
                                };
                                launch_via_gateway =
                                    gateway_pack::governed_launch_after_watch_reconciliation(
                                        true,
                                        reconciliation.is_ok(),
                                    );
                                if launch_via_gateway {
                                    let _ = auto_start_handle.emit(
                                        "council-log",
                                        "[system] auto-start: restoring governed route — Gateway Pack and Watch/Outbox auth revalidated",
                                    );
                                } else if let Err(e) = reconciliation {
                                    let _ = auto_start_handle.emit(
                                        "council-log",
                                        format!(
                                            "[system] auto-start: Watch-token reconciliation failed ({e}); starting Council in Direct mode"
                                        ),
                                    );
                                }
                            } else {
                                let _ = auto_start_handle.emit(
                                    "council-log",
                                    "[system] auto-start: pack not immediately ready — attempting bounded resume (compose up + health/auth wait)",
                                );
                                let resume = match preloaded {
                                    Some((key, secrets)) => gateway_pack::resume_installed_pack_with_key(
                                        &store, key, secrets,
                                    ),
                                    None => Err(
                                        "cold-launch Keychain preload unavailable".to_string(),
                                    ),
                                };
                                match resume {
                                    Ok(()) => {
                                        launch_via_gateway = true;
                                        let _ = auto_start_handle.emit(
                                            "council-log",
                                            "[system] auto-start: pack resume succeeded — spawning governed Council",
                                        );
                                    }
                                    Err(e) => {
                                        let _ = auto_start_handle.emit(
                                            "council-log",
                                            format!(
                                                "[system] auto-start: pack resume failed ({e}); starting Council in Direct mode (via_gateway_default kept; bounded promote will retry)"
                                            ),
                                        );
                                    }
                                }
                            }
                        }
                        let mut first_attempt = true;
                        let mut route = launch_via_gateway;
                        loop {
                            match try_start_council_server_with_credentials(
                                &auto_start_handle,
                                None,
                                token_ref,
                                Some(route),
                                None,
                                if route {
                                    preloaded_gateway_creds.as_ref()
                                } else {
                                    None
                                },
                            ) {
                                Ok(msg) => {
                                    let _ = auto_start_handle.emit(
                                        "council-log",
                                        format!("[system] auto-start: {msg}"),
                                    );
                                    break;
                                }
                                Err(e) => {
                                    if first_attempt && route {
                                        // Governed spawn failed after a
                                        // successful revalidation: fall back to
                                        // Direct so core War Room still comes
                                        // up. gateway_pack_status reports the
                                        // pack truth (child recorded Direct).
                                        // Do not clear via_gateway_default.
                                        first_attempt = false;
                                        route = false;
                                        let _ = auto_start_handle.emit(
                                            "council-log",
                                            format!("[system] auto-start: governed start failed ({e}); falling back to Direct"),
                                        );
                                        continue;
                                    }
                                    let extra = if packaged {
                                        String::new()
                                    } else {
                                        " (run `cargo build --release` at council-rs root)".to_string()
                                    };
                                    let _ = auto_start_handle.emit(
                                        "council-log",
                                        format!("[system] auto-start skipped: {e}{extra}"),
                                    );
                                    break;
                                }
                            }
                        }
                        // Fail-closed Direct with pack still enabled: schedule
                        // bounded later promote without requiring manual Enable.
                        if packaged
                            && persisted_via_gateway
                            && gateway_pack::owned_council_route() == Some(false)
                        {
                            schedule_governed_promote_attempts(
                                auto_start_handle.clone(),
                                auth_token.clone(),
                                launch_key.clone(),
                                launch_secrets.clone(),
                            );
                        }
                    }
                    // Refresh live authenticated flag after resume/spawn; no
                    // Keychain re-get (held key). Early seed already covered
                    // the UI race window during pack bring-up. Rejects if
                    // generation advanced since the preload flight.
                    gateway_pack::seed_auth_observation_from_preloaded_key(
                        launch_key.as_deref(),
                        preload_auth_generation,
                    );
                    // Start presentation polling only after cold launch has
                    // seeded both presence caches. Background ticks perform
                    // zero Keychain reads.
                    status_authority::start_background_loop(auto_start_handle.clone());
                });
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while running tauri application");

    app.run(|app_handle, event| {
        if matches!(event, RunEvent::Exit | RunEvent::ExitRequested { .. }) {
            stop_tracked_council_server(app_handle);
        }
    });
}

#[cfg(test)]
mod runtime_mode_tests {
    use super::{
        desktop_runtime_config_value, desktop_runtime_mode_value, should_reveal_main_window,
        validate_runtime_ready_port,
    };
    use tauri::webview::PageLoadEvent;

    #[test]
    fn runtime_mode_matches_the_native_build_profile() {
        let expected = if cfg!(debug_assertions) {
            "development"
        } else {
            "installed-release"
        };
        assert_eq!(desktop_runtime_mode_value(), expected);
    }

    #[test]
    fn desktop_runtime_config_uses_the_selected_loopback_port() {
        assert_eq!(
            desktop_runtime_config_value(20_321),
            serde_json::json!({
                "apiBase": "http://127.0.0.1:20321",
                "wsBase": "ws://127.0.0.1:20321",
            })
        );
    }

    #[test]
    fn runtime_ready_receipt_accepts_only_the_selected_port() {
        assert!(validate_runtime_ready_port(20_321, 20_321).is_ok());
        assert!(validate_runtime_ready_port(8_765, 20_321).is_err());
    }

    #[test]
    fn initial_main_window_reveals_only_after_page_load_finishes() {
        assert!(!should_reveal_main_window("main", PageLoadEvent::Started));
        assert!(should_reveal_main_window("main", PageLoadEvent::Finished));
        assert!(!should_reveal_main_window(
            "secondary",
            PageLoadEvent::Finished
        ));
    }
}

#[cfg(test)]
mod council_start_phase_tests {
    /// Innermost free-function call name after stripping `.await` / `?` wrappers.
    fn free_fn_call_name(expr: &syn::Expr) -> Option<&syn::Ident> {
        match expr {
            syn::Expr::Try(t) => free_fn_call_name(&t.expr),
            syn::Expr::Await(a) => free_fn_call_name(&a.base),
            syn::Expr::Call(c) => match c.func.as_ref() {
                syn::Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
                    Some(&p.path.segments[0].ident)
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Phase helper invoked by one orchestrator statement, if any.
    fn phase_call_from_stmt(stmt: &syn::Stmt) -> Option<&syn::Ident> {
        match stmt {
            syn::Stmt::Local(local) => local
                .init
                .as_ref()
                .and_then(|init| free_fn_call_name(&init.expr)),
            // Tail expression (e.g. bare call without `;`).
            syn::Stmt::Expr(expr, _) => free_fn_call_name(expr),
            _ => None,
        }
    }

    #[test]
    fn council_start_phases_are_named_and_ordered() {
        // PR6: pin orchestrator runtime phase order from the AST, not source text.
        // Comments and string/raw-string literals are not statements, so they cannot
        // spoof or shadow executable call order the way line/substring matching can.
        let file: syn::File =
            syn::parse_file(include_str!("lib.rs")).expect("lib.rs must parse as a Rust file");

        let orch = file
            .items
            .into_iter()
            .find_map(|item| match item {
                syn::Item::Fn(f) if f.sig.ident == "try_start_council_server_with_credentials" => {
                    Some(f)
                }
                _ => None,
            })
            .expect("orchestrator try_start_council_server_with_credentials must exist in lib.rs");

        let expected = [
            "resolve_start_plan",
            "ensure_council_port_available",
            "resolve_spawn_layout",
            "resolve_gateway_authority",
            "spawn_and_register_council",
        ];
        let observed: Vec<String> = orch
            .block
            .stmts
            .iter()
            .filter_map(phase_call_from_stmt)
            .filter(|id| expected.iter().any(|name| id == name))
            .map(|id| id.to_string())
            .collect();

        assert_eq!(
            observed, expected,
            "orchestrator must invoke the five phases as statements in this order \
             (AST statement order, not textual line matching)"
        );
    }
}
