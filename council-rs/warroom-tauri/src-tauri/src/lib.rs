// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/

#[cfg(test)]
#[path = "../build_support.rs"]
mod build_support;
mod docker_cli;
mod gateway_pack;
mod keychain;
mod paths;
mod private_config;
mod sidecar;

use gateway_pack::{GatewayPackState, GatewayPackStatus};
use keychain::{load_gw_api_key, migrate_legacy_secrets, KeychainSecretStore};
use paths::{
    build_cors_origins, default_serve_port, is_packaged_install, resolve_council_binary,
    resolve_council_rs_dir, resolve_spawn_base_dir, validate_serve_port,
};
use private_config::{
    ensure_writable_base_overlay, gui_login_environment, load_or_create_private_config,
};
use sidecar::{
    compose_sidecar_args, compose_sidecar_env, probe_council_server, validate_council_root,
    wait_for_port_release, CouncilServerProbe, GatewayChildCredentials,
};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{TrayIconBuilder, TrayIconEvent},
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
    council_path: Option<String>,
    server_port: Option<u16>,
    auth_token: Option<String>,
    council_root: Option<String>,
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

/// Best-effort kill of the tracked council sidecar (shared by stop command, tray, and app exit).
///
/// Prefer an orderly kill of the owned child, then re-check the PID so a stuck
/// listener is not left reparented under launchd/PID 1 when the host exits.
/// Never kills a process that is not the tracked child PID.
fn stop_tracked_council_server(app: &AppHandle) {
    let state = app.state::<CouncilServer>();
    let mut tracked_pid: Option<u32> = None;
    if let Ok(mut guard) = state.0.lock() {
        if let Some(child) = guard.child.take() {
            tracked_pid = Some(child.pid());
            let _ = child.kill();
        }
    };
    if let Some(pid) = tracked_pid {
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
        // Best-effort listener release after owned child death.
        let _ = wait_for_port_release(default_serve_port().unwrap_or(8765), Duration::from_secs(3));
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

/// Adopt a matching external Council when available; otherwise spawn a managed
/// `council --serve` when this is a packaged install (bundled binary + base-dir)
/// or a debug development build. Packaged releases own the bundled Council
/// lifecycle and do not require `make setup`, Rust, Node, or Docker for core
/// War Room. Gateway remains optional (`via_gateway` default off).
///
/// `via_gateway`: `Some(_)` sets `COUNCIL_VIA_GATEWAY` explicitly ("1"/"0"); `None` inherits
/// (packaged installs force `Some(false)` unless the caller opts in).
/// `council_root`: Settings councilRoot — when non-blank it must pass
/// `validate_council_root` and replaces the `--base-dir` value only.
fn try_start_council_server(
    app: &AppHandle,
    council_path: Option<&str>,
    server_port: Option<u16>,
    auth_token: Option<&str>,
    via_gateway: Option<bool>,
    council_root: Option<&str>,
    librarian_base: Option<&str>,
) -> Result<String, String> {
    let state = app.state::<CouncilServer>();
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    if guard.child.is_some() {
        return Ok("council server already tracked as running".to_string());
    }

    let port = match server_port {
        Some(port) => port,
        None => default_serve_port()?,
    };
    validate_serve_port(port)?;
    eprintln!("[council-runtime] start requested on :{port}");

    let packaged = is_packaged_install();

    // Validate the base-dir override before spawning — a bad base dir makes
    // council exit at startup and the failure would only surface via the log
    // pump while this fn reports Ok. Whitespace-only is treated as absent
    // (same as the councilPath precedent in resolve_council_binary).
    let council_root = council_root.map(str::trim).filter(|s| !s.is_empty());
    let base_dir_override = council_root.map(validate_council_root).transpose()?;
    let (expected_sha, expected_dirty) = bundled_build_identity();
    match probe_council_server(
        port,
        Duration::from_millis(750),
        expected_sha,
        expected_dirty,
        auth_token,
    ) {
        CouncilServerProbe::MatchingBuild => {
            eprintln!("[council-runtime] adopted exact build on :{port}");
            // Adopted, not owned: we did not spawn this process, so its
            // route is unproven and status must not claim governed from it.
            gateway_pack::record_owned_council_route(None);
            return Ok(format!(
                "adopted Council already running on :{port} (matching build identity)"
            ));
        }
        CouncilServerProbe::DifferentBuild => {
            return Err(format!(
                "Council on :{port} has a different source identity; quit the other Council \
                 process or free the port before launching this app (this app will not kill it)"
            ));
        }
        CouncilServerProbe::Unavailable => {
            if !wait_for_port_release(port, Duration::from_millis(0)) {
                return Err(format!(
                    "port {port} is occupied by a non-canonical or unhealthy process"
                ));
            }
        }
    }

    // Packaged release owns the bundled sidecar. Debug owns a repo-built sidecar.
    // Unpackaged release (dev shell without bundle) still requires an external runtime.
    if !packaged && !cfg!(debug_assertions) {
        return Err(
            "Council is not running and this build is not a self-contained app bundle. \
             Use the DMG-packaged app, or start Council externally."
                .to_string(),
        );
    }

    let effective = resolve_council_binary(council_path)?;
    let effective = effective.to_string_lossy().into_owned();

    // Packaged: writable Application Support overlay of bundled base-dir (cabinets save).
    // Dev: council-rs repo root (or validated override).
    let spawn_base = if let Some(ref override_path) = base_dir_override {
        override_path.clone()
    } else if packaged {
        let bundled = paths::bundled_base_dir().ok_or_else(|| {
            "packaged install is missing Contents/Resources/council-base".to_string()
        })?;
        ensure_writable_base_overlay(&bundled)?
    } else {
        resolve_spawn_base_dir(None)?
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

    // Packaged installs default Gateway off so missing Docker cannot break core War Room.
    let via_gateway = if packaged {
        Some(via_gateway.unwrap_or(false))
    } else {
        via_gateway
    };

    // Keychain-sourced GW_API_KEY for governed mode only. Never from login shell.
    let gateway_creds: Option<GatewayChildCredentials> = if via_gateway == Some(true) {
        let store = KeychainSecretStore;
        match load_gw_api_key(&store) {
            Ok(Some(api_key)) => {
                // Packaged installs spawn governed only on proven pack-side
                // authentication — the same requirement restart_sidecar
                // enforces with the full AuthenticatedReady state. The spawn
                // itself creates the governed-child proof, so this gate uses
                // the status-level predicate (enabled + live-authenticated),
                // which the enable and relaunch-restore flows satisfy right
                // after their own revalidation and a Disabled pack never does.
                if packaged {
                    let st = gateway_pack::gateway_pack_status(&store);
                    if !st.allows_governed_spawn() {
                        return Err(format!(
                            "Gateway is not authenticated-ready ({}). {}",
                            st.state.as_str(),
                            st.message
                        ));
                    }
                }
                Some(GatewayChildCredentials {
                    api_key,
                    gateway_url: docker_cli::DESKTOP_GATEWAY_URL.to_string(),
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
    } else {
        None
    };

    let cors_origins = build_cors_origins(port);
    // compose_sidecar_args: first arg is default base-dir; third overrides --base-dir.
    let args = compose_sidecar_args(&spawn_base_str, port, Some(spawn_base_str.as_str()));

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
    // Apply login env first, then compose_sidecar_env scrub/inject wins for Gateway vars
    // when re-applied below after login merge.
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
                    // Only clear the child this pump belongs to — a stale
                    // Terminated from a killed child must not untrack a respawn.
                    if server_guard.generation == spawn_generation {
                        server_guard.child = None;
                        // Owned child is gone; its route proof dies with it.
                        gateway_pack::record_owned_council_route(None);
                    }
                };
            }
        }
    });

    guard.child = Some(child);
    // Prove the owned child's route for gateway_pack_status: only a spawn
    // with COUNCIL_VIA_GATEWAY=1 (Keychain creds) counts as governed. Record
    // while still holding the server guard so the log pump's Terminated
    // cleanup cannot interleave between the child store and this record.
    gateway_pack::record_owned_council_route(Some(via_gateway == Some(true)));
    drop(guard);

    // Cache the spawn config so restart_sidecar can respawn with the same
    // council path + pairing token + council root (token is not re-sent by
    // the frontend). council_root is cached as the trimmed user value, not the
    // canonicalized path, so restart re-validates against the live filesystem.
    {
        let config_state = app.state::<SpawnConfigCache>();
        if let Ok(mut config_guard) = config_state.0.lock() {
            *config_guard = LastSpawnConfig {
                council_path: council_path.map(str::to_string),
                server_port: Some(port),
                auth_token: auth_token.map(str::to_string),
                council_root: council_root.map(str::to_string),
                librarian_base: librarian_base.map(str::to_string),
            };
        };
    }

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

/// Start or adopt Council for the desktop shell.
/// Packaged installs spawn the bundled sidecar; debug builds may spawn a repo
/// binary; matching external Council is adopted when already healthy.
/// Sets `COUNCIL_CORS_ORIGINS` for Tauri asset origins and Next dev (3010) / API port.
/// `COUNCIL_DEV_NO_AUTH` is set only in debug builds; release requires `COUNCIL_AUTH_TOKEN`.
/// The default port is selected at build time from `IRIN_COUNCIL_PORT` in
/// isolated worktrees and remains 8765 for the canonical installed runtime.
/// `council_root` (Settings councilRoot, camelCase over invoke) overrides
/// `--base-dir` after validation; blank/absent uses bundled base-dir or repo root.
#[tauri::command]
async fn start_council_server(
    app: AppHandle,
    council_path: Option<String>,
    server_port: Option<u16>,
    auth_token: Option<String>,
    council_root: Option<String>,
    librarian_base: Option<String>,
) -> Result<String, String> {
    try_start_council_server(
        &app,
        council_path.as_deref(),
        server_port,
        auth_token.as_deref(),
        None,
        council_root.as_deref(),
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
/// Packaged installs refuse `via_gateway=true` unless the Gateway Pack reports
/// authenticated readiness (Keychain key + live `/v1/models`).
/// `council_root`: optional fresh `--base-dir` override — unlike the pairing
/// token it can change mid-session, so the restart accepts the form value
/// instead of trusting the cache; `None` falls back to the cached spawn value.
#[tauri::command]
async fn restart_sidecar(
    app: AppHandle,
    via_gateway: bool,
    council_root: Option<String>,
    librarian_base: Option<String>,
) -> Result<String, String> {
    // Port-release polling blocks (up to 5s) — keep it off the async runtime.
    tauri::async_runtime::spawn_blocking(move || {
        if via_gateway && is_packaged_install() {
            let store = KeychainSecretStore;
            let st = gateway_pack::gateway_pack_status(&store);
            if !st.state.allows_governed() {
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

        // Validate the effective council root BEFORE killing the running
        // sidecar — an invalid path must leave the current backend untouched
        // rather than tearing it down and failing the respawn.
        let effective_root = council_root
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| config.council_root.clone());
        effective_root
            .as_deref()
            .map(validate_council_root)
            .transpose()?;

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
                CouncilServerProbe::MatchingBuild => {
                    return Err(
                        "Council is managed by the external IRIN runtime; restart it with `make runtime-restart`"
                            .to_string(),
                    );
                }
                CouncilServerProbe::DifferentBuild => {
                    return Err(format!(
                        "Council on :{port} has a different source identity; run `make setup` and rebuild this app from the same checkout"
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
            config.council_path.as_deref(),
            Some(port),
            config.auth_token.as_deref(),
            Some(via_gateway),
            effective_root.as_deref(),
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
#[tauri::command]
async fn gateway_pack_enable(app: AppHandle) -> Result<GatewayPackStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let store = KeychainSecretStore;
        let status = gateway_pack::enable_gateway_pack(&store)?;
        // Docker missing/down is neutral for core Direct — return status as-is.
        if matches!(
            status.state,
            GatewayPackState::DockerMissing | GatewayPackState::DockerDaemonDown
        ) {
            return Ok(status);
        }
        if !status.authenticated || !status.enabled {
            return Ok(status);
        }

        // Restart owned Council child into governed mode with Keychain key.
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
        if !had_child {
            // No owned child: pack auth alone is not full ready for this shell.
            return Ok(gateway_pack::status_with_council_route(
                &store, false, false,
            ));
        }
        stop_tracked_council_server(&app);
        let _ = wait_for_port_release(default_serve_port().unwrap_or(8765), Duration::from_secs(5));
        match try_start_council_server(
            &app,
            config.council_path.as_deref(),
            None,
            config.auth_token.as_deref(),
            Some(true),
            config.council_root.as_deref(),
            config.librarian_base.as_deref(),
        ) {
            Ok(msg) => {
                let _ = app.emit("council-log", format!("[system] gateway enable: {msg}"));
                Ok(gateway_pack::status_with_council_route(
                    &store, true, false,
                ))
            }
            Err(e) => {
                let _ = app.emit(
                    "council-log",
                    format!("[system] gateway enable: council governed restart failed: {e}"),
                );
                // Roll back: via_gateway_default=true was persisted and the
                // working Direct child was already stopped. Restore the
                // persisted route to Direct, then try to bring core War Room
                // back up before returning the enable error — never leave the
                // app down with state claiming governed.
                if let Err(disable_err) = gateway_pack::disable_gateway_pack(&store) {
                    let _ = app.emit(
                        "council-log",
                        format!("[system] gateway enable rollback: failed to restore Direct config: {disable_err}"),
                    );
                }
                let _ = wait_for_port_release(
                    default_serve_port().unwrap_or(8765),
                    Duration::from_secs(5),
                );
                let rollback_note = match try_start_council_server(
                    &app,
                    config.council_path.as_deref(),
                    None,
                    config.auth_token.as_deref(),
                    Some(false),
                    config.council_root.as_deref(),
                    config.librarian_base.as_deref(),
                ) {
                    Ok(msg) => {
                        let _ = app.emit(
                            "council-log",
                            format!("[system] gateway enable rollback: Council restored in Direct mode: {msg}"),
                        );
                        "Council was restored in Direct mode.".to_string()
                    }
                    Err(re) => {
                        let _ = app.emit(
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
async fn gateway_pack_disable(app: AppHandle) -> Result<GatewayPackStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let store = KeychainSecretStore;
        let _status = gateway_pack::disable_gateway_pack(&store)?;
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
        if had_child {
            stop_tracked_council_server(&app);
            let _ =
                wait_for_port_release(default_serve_port().unwrap_or(8765), Duration::from_secs(5));
            try_start_council_server(
                &app,
                config.council_path.as_deref(),
                None,
                config.auth_token.as_deref(),
                Some(false),
                config.council_root.as_deref(),
                config.librarian_base.as_deref(),
            )
            .map_err(|e| format!("Gateway disabled but Council Direct restart failed: {e}"))?;
            let _ = app.emit(
                "council-log",
                "[system] gateway disable: Council restarted in Direct mode",
            );
            Ok(gateway_pack::status_with_council_route(&store, false, true))
        } else {
            Ok(gateway_pack::status_with_council_route(&store, false, true))
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Stop the desktop Compose project only (no volume delete).
/// Switches to Direct first (via stop_gateway_pack) and restarts owned Council in Direct.
#[tauri::command]
async fn gateway_pack_stop(app: AppHandle) -> Result<GatewayPackStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let store = KeychainSecretStore;
        // Ensure Direct config before containers stop.
        let status = gateway_pack::stop_gateway_pack(&store)?;
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
        if had_child {
            stop_tracked_council_server(&app);
            let _ =
                wait_for_port_release(default_serve_port().unwrap_or(8765), Duration::from_secs(5));
            try_start_council_server(
                &app,
                config.council_path.as_deref(),
                None,
                config.auth_token.as_deref(),
                Some(false),
                config.council_root.as_deref(),
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
        Ok(st)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Destructive uninstall of the desktop pack only. Explicit operator action.
/// Propagates Council Direct restart failures.
#[tauri::command]
async fn gateway_pack_uninstall(app: AppHandle) -> Result<GatewayPackStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let store = KeychainSecretStore;
        let status = gateway_pack::uninstall_gateway_pack(&store)?;
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
        if had_child {
            stop_tracked_council_server(&app);
            let _ =
                wait_for_port_release(default_serve_port().unwrap_or(8765), Duration::from_secs(5));
            try_start_council_server(
                &app,
                config.council_path.as_deref(),
                None,
                config.auth_token.as_deref(),
                Some(false),
                config.council_root.as_deref(),
                config.librarian_base.as_deref(),
            )
            .map_err(|e| {
                format!("Gateway pack uninstalled but Council Direct restart failed: {e}")
            })?;
        }
        let mut st = status;
        st.council_governed = false;
        st.message = "Gateway pack uninstalled; Council is in Direct mode.".into();
        Ok(st)
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
            pick_file,
            ping_council,
            get_server_logs,
            clear_server_logs,
            save_synthesis,
            save_pdf,
            desktop_runtime_mode,
            desktop_runtime_config,
            report_council_runtime_ready
        ])
        .setup(|app| {
            // One-time, non-destructive adoption of Keychain secrets stored by
            // the legacy "Council War Room" build (never deletes legacy items).
            {
                let store = KeychainSecretStore;
                migrate_legacy_secrets(&store);
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
                        // ONLY after revalidating pack authentication (Docker
                        // up, owned pack running + healthy, Keychain key passes
                        // /v1/models). Anything less starts Direct explicitly —
                        // status must never claim governed while the owned
                        // child is Direct.
                        let mut launch_via_gateway = false;
                        if packaged && persisted_via_gateway {
                            let store = KeychainSecretStore;
                            if gateway_pack::pack_auth_revalidated(&store) {
                                launch_via_gateway = true;
                                let _ = auto_start_handle.emit(
                                    "council-log",
                                    "[system] auto-start: restoring governed route — Gateway Pack revalidated (Docker up, pack authenticated, Keychain key usable)",
                                );
                            } else {
                                let _ = auto_start_handle.emit(
                                    "council-log",
                                    "[system] auto-start: persisted governed route not restored — Gateway Pack unavailable or not authenticated; starting Council in Direct mode",
                                );
                            }
                        }
                        let mut first_attempt = true;
                        let mut route = launch_via_gateway;
                        loop {
                            match try_start_council_server(
                                &auto_start_handle,
                                None,
                                None,
                                token_ref,
                                Some(route),
                                None,
                                None,
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
                    }
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
        desktop_runtime_config_value, desktop_runtime_mode_value, validate_runtime_ready_port,
    };

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
}
