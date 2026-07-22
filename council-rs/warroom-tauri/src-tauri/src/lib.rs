// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/

#[cfg(test)]
#[path = "../build_support.rs"]
mod build_support;
mod paths;
mod sidecar;

use paths::{
    build_cors_origins, resolve_council_binary, resolve_council_rs_dir, validate_serve_port,
    DEFAULT_SERVE_PORT,
};
use sidecar::{
    compose_sidecar_args, compose_sidecar_env, probe_council_server, validate_council_root,
    wait_for_port_release, CouncilServerProbe,
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
fn stop_tracked_council_server(app: &AppHandle) {
    let state = app.state::<CouncilServer>();
    if let Ok(mut guard) = state.0.lock() {
        if let Some(child) = guard.child.take() {
            let _ = child.kill();
        }
    };
}

/// Adopt an external Council when available. Debug builds may spawn
/// `council --serve` for development; installed release builds never create a
/// second backend and require the canonical IRIN runtime from `make setup`.
/// `via_gateway`: `Some(_)` sets `COUNCIL_VIA_GATEWAY` explicitly ("1"/"0"); `None` inherits.
/// `council_root`: Settings councilRoot — when non-blank it must pass
/// `validate_council_root` and replaces the `--base-dir` value only (binary
/// resolution and the spawn cwd stay pinned to the repo root).
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

    let port = server_port.unwrap_or(DEFAULT_SERVE_PORT);
    validate_serve_port(port)?;

    let council_rs_path = resolve_council_rs_dir();

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
            return Ok(format!(
                "adopted canonical Council already running on :{port} (managed by external IRIN runtime)"
            ));
        }
        CouncilServerProbe::DifferentBuild => {
            return Err(format!(
                "Council on :{port} has a different source identity; run `make setup` and rebuild this app from the same checkout"
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

    if !cfg!(debug_assertions) {
        return Err(
            "canonical IRIN Council is not running; run `make setup` from the IRIN checkout"
                .to_string(),
        );
    }

    let effective = resolve_council_binary(council_path)?;
    let effective = effective.to_string_lossy().into_owned();
    let council_rs_dir = council_rs_path.to_string_lossy().into_owned();
    let base_dir_override = base_dir_override.map(|p| p.to_string_lossy().into_owned());

    let cors_origins = build_cors_origins(port);
    let args = compose_sidecar_args(&council_rs_dir, port, base_dir_override.as_deref());

    let mut command = app
        .shell()
        .command(&effective)
        .current_dir(&council_rs_dir)
        .args(args);
    for (key, value) in compose_sidecar_env(
        cors_origins.as_str(),
        cfg!(debug_assertions),
        auth_token,
        via_gateway,
        librarian_base,
    ) {
        command = command.env(key, value);
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
                    }
                };
            }
        }
    });

    guard.child = Some(child);
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
                auth_token: auth_token.map(str::to_string),
                council_root: council_root.map(str::to_string),
                librarian_base: librarian_base.map(str::to_string),
            };
        };
    }

    let _ = app
        .notification()
        .builder()
        .title("Council War Room")
        .body(format!("Sidecar council --serve started on :{port}"))
        .show();

    Ok(format!(
        "council --serve started on :{} (bin: {}{}). WS/REST should be reachable from Tauri webview",
        port,
        effective,
        base_dir_override
            .as_deref()
            .map(|d| format!(", base-dir: {d}"))
            .unwrap_or_default()
    ))
}

/// Adopt the canonical Council in release builds; start a managed sidecar only
/// in a debug build used for desktop development.
/// Sets `COUNCIL_CORS_ORIGINS` for Tauri asset origins and Next dev (3010) / API port.
/// `COUNCIL_DEV_NO_AUTH` is set only in debug builds; release requires `COUNCIL_AUTH_TOKEN`.
/// Only port **8765** is accepted until a runtime config bridge exists.
/// `council_root` (Settings councilRoot, camelCase over invoke) overrides
/// `--base-dir` after validation; blank/absent uses the repo root.
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
/// Kills the tracked child (if any), waits for :8765 to free up, and respawns
/// with `COUNCIL_VIA_GATEWAY=1` when `via_gateway` is true ("0" when false —
/// explicit off, since the child inherits the parent env). Reuses the cached
/// spawn config so the pairing token survives the restart; if no sidecar is
/// tracked this simply starts one. Returns the same shape as start_council_server.
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
        if !had_child {
            let (expected_sha, expected_dirty) = bundled_build_identity();
            match probe_council_server(
                DEFAULT_SERVE_PORT,
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
                    return Err(
                        "Council on :8765 has a different source identity; run `make setup` and rebuild this app from the same checkout"
                            .to_string(),
                    );
                }
                CouncilServerProbe::Unavailable => {
                    if !wait_for_port_release(DEFAULT_SERVE_PORT, Duration::from_millis(0)) {
                        return Err(
                            "port 8765 is occupied by a non-canonical or unhealthy process"
                                .to_string(),
                        );
                    }
                }
            }
        }
        stop_tracked_council_server(&app);
        if had_child {
            // kill() returns before the OS releases the listener; wait so the
            // respawn does not lose the bind race on :8765.
            if !wait_for_port_release(DEFAULT_SERVE_PORT, Duration::from_secs(5)) {
                let _ = app.emit(
                    "council-log",
                    format!(
                        "[system] restart: port {DEFAULT_SERVE_PORT} still busy after 5s; spawning anyway"
                    ),
                );
            }
        }

        try_start_council_server(
            &app,
            config.council_path.as_deref(),
            None,
            config.auth_token.as_deref(),
            Some(via_gateway),
            effective_root.as_deref(),
            librarian_base.as_deref().or(config.librarian_base.as_deref()),
        )
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
            pick_file,
            ping_council,
            get_server_logs,
            clear_server_logs,
            save_synthesis,
            save_pdf,
            desktop_runtime_mode
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let menu = Menu::with_items(
                app,
                &[
                    &MenuItem::with_id(app, "show", "Open War Room", true, None::<&str>)?,
                    &MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?,
                ],
            )?;
            let mut tray_builder = TrayIconBuilder::new()
                .tooltip("Council War Room")
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

            // Debug-only Rust auto-start (COUNCIL_DEV_NO_AUTH). Release builds rely on
            // the webview calling start_council_server with Settings auth after configReady.
            #[cfg(debug_assertions)]
            {
                let auto_start_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    match try_start_council_server(&auto_start_handle, None, None, None, None, None, None)
                    {
                        Ok(msg) => {
                            let _ = auto_start_handle.emit(
                                "council-log",
                                format!("[system] auto-start: {msg}"),
                            );
                        }
                        Err(e) => {
                            let _ = auto_start_handle.emit(
                                "council-log",
                                format!(
                                    "[system] auto-start skipped: {e} (run `cargo build --release` at council-rs root)"
                                ),
                            );
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
    use super::desktop_runtime_mode_value;

    #[test]
    fn runtime_mode_matches_the_native_build_profile() {
        let expected = if cfg!(debug_assertions) {
            "development"
        } else {
            "installed-release"
        };
        assert_eq!(desktop_runtime_mode_value(), expected);
    }
}
