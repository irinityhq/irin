/**
 * Tauri desktop bridge — all imports are dynamic so `next build` works in browser-only mode.
 */

import {
  getAuthToken,
  councilPortFromApiBase,
  getLibrarianBase,
  getRuntimeConfig,
} from "./runtime-config";

export type DesktopRuntimeMode = "development" | "installed-release";

function tauriWindow(): (Window & { __TAURI__?: unknown }) | null {
  if (typeof window === "undefined") return null;
  const w = window as Window & {
    __TAURI__?: unknown;
    __TAURI_INTERNALS__?: unknown;
  };
  // `__TAURI__` only exists with `app.withGlobalTauri: true`;
  // `__TAURI_INTERNALS__` is injected into every Tauri v2 webview.
  return w.__TAURI__ || w.__TAURI_INTERNALS__ ? w : null;
}

export function isTauri(): boolean {
  return tauriWindow() !== null;
}

/** Native build profile; installed releases cannot own a debug Council sidecar. */
export async function getDesktopRuntimeMode(): Promise<DesktopRuntimeMode> {
  return invoke<DesktopRuntimeMode>("desktop_runtime_mode");
}

/**
 * Packaged installs: native setup is the sole Council startup owner.
 * Source-dev returns false so the frontend may still call startCouncilServer.
 */
export async function nativeOwnsCouncilStartup(): Promise<boolean> {
  if (!isTauri()) return false;
  return invoke<boolean>("native_owns_council_startup");
}

async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const { invoke: inv } = await import("@tauri-apps/api/core");
  return inv<T>(cmd, args);
}

/** Subscribe to sidecar stdout/stderr lines. Returns unsubscribe. */
export async function onCouncilLog(
  cb: (line: string) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  const unlisten = await listen<string>("council-log", (ev) => {
    if (typeof ev.payload === "string") cb(ev.payload);
  });
  return unlisten;
}

export async function startCouncilServer(
  serverPort?: number,
  authToken?: string,
  librarianBase?: string,
): Promise<string> {
  const token = authToken ?? getAuthToken();
  const libBase = (librarianBase ?? getLibrarianBase()).trim();
  const resolvedPort =
    serverPort ?? councilPortFromApiBase(getRuntimeConfig().apiBase);
  return invoke<string>("start_council_server", {
    serverPort: resolvedPort,
    authToken: token.trim() ? token.trim() : null,
    librarianBase: libBase || null,
  });
}

/** Record that the embedded webview completed its initial Council requests. */
export async function reportCouncilRuntimeReady(port: number): Promise<void> {
  await invoke("report_council_runtime_ready", { port });
}

export async function stopCouncilServer(): Promise<string> {
  return invoke<string>("stop_council_server");
}

/**
 * Kill the tracked sidecar and respawn `council --serve` with
 * `COUNCIL_VIA_GATEWAY=1` when `viaGateway` is true (sets
 * `COUNCIL_VIA_GATEWAY=0` when false — the child inherits the parent env, so
 * an unset var could leak gateway mode; see compose_sidecar_env).
 * Note: the respawned sidecar exits at startup if `GW_API_KEY` is missing or
 * the gateway health check fails — watch the backend log panel after restart.
 */
export async function restartSidecar(
  viaGateway: boolean,
  librarianBase?: string,
): Promise<string> {
  return invoke<string>("restart_sidecar", {
    viaGateway,
    librarianBase: librarianBase || null,
  });
}

/** Truthful Gateway Pack states from the native host (never secret-bearing). */
export type GatewayPackState =
  | "not_installed"
  | "docker_missing"
  | "docker_daemon_down"
  | "installing"
  | "installed_stopped"
  | "starting"
  | "authenticated_ready"
  | "degraded"
  | "disabled";

export interface GatewayPackStatus {
  state: GatewayPackState;
  message: string;
  pack_version: string | null;
  manifest_mode: string | null;
  gateway_url: string;
  project: string;
  key_id: string | null;
  enabled: boolean;
  docker: string;
  watch_producer_enabled: boolean;
  watch_dispatcher_enabled: boolean;
  authenticated: boolean;
  /** True when Gateway auth + Council governed route are both proven. */
  council_governed: boolean;
  /** Fixed pack URL is configured (distinct from authenticated-ready). */
  gateway_url_configured: boolean;
  support_matrix_summary: string;
  /** Pack enabled + live-authenticated — enough to spawn a governed child. */
  spawn_capable: boolean;
  /** Full governed readiness; Deliberate toggle and enroll/arm gates. */
  governed_ready: boolean;
  /** Structural hard-down for presentation demotion. */
  hard_down: boolean;
}

/**
 * Whether governed proceedings may start. Reads the native-serialized field
 * only — do not re-derive from state + authenticated + council_governed.
 */
export function gatewayPackAllowsGoverned(
  status: GatewayPackStatus | null | undefined,
): boolean {
  return status?.governed_ready === true;
}

export async function getGatewayPackStatus(): Promise<GatewayPackStatus> {
  return invoke<GatewayPackStatus>("gateway_pack_status");
}

export async function enableGatewayPack(): Promise<DesktopStatusSnapshot> {
  return invoke<DesktopStatusSnapshot>("gateway_pack_enable");
}

export async function disableGatewayPack(): Promise<DesktopStatusSnapshot> {
  return invoke<DesktopStatusSnapshot>("gateway_pack_disable");
}

export async function stopGatewayPack(): Promise<DesktopStatusSnapshot> {
  return invoke<DesktopStatusSnapshot>("gateway_pack_stop");
}

export async function uninstallGatewayPack(): Promise<DesktopStatusSnapshot> {
  return invoke<DesktopStatusSnapshot>("gateway_pack_uninstall");
}

/** Renderer-safe projection of the native Touch ID ceremony. */
export type TouchIdState =
  | "unavailable"
  | "blocked"
  | "setup_required"
  | "reenroll_required"
  | "ready"
  | "ceremony_open"
  | "armed";

export type TouchIdReason =
  | "helper_missing"
  | "gateway_not_ready"
  | "watch_surface_unreachable"
  | "arm_principal_missing"
  | "registry_unloaded"
  | "registry_mismatch"
  | "helper_identity_changed"
  | "enclave_key_missing"
  | "enrollment_missing"
  | "rehearsal_only_build"
  | "lease_expired";

export interface TouchIdStatus {
  state: TouchIdState;
  reason: TouchIdReason | null;
  armed_exp_at_ms: number | null;
  armed_expires_in_ms: number | null;
  stage_expires_in_ms: number | null;
  enrolled: boolean;
  allow_real_arm: boolean;
  can_enroll: boolean;
  can_arm: boolean;
  can_renew: boolean;
  can_disarm: boolean;
  /** Last successful ceremony was rehearsal-ok (producer did not start). */
  rehearsal_passed: boolean;
}

/** Host-authoritative combined status snapshot (ordered by seq). */
export interface DesktopStatusSnapshot {
  authority_epoch: string;
  seq: number;
  pack: GatewayPackStatus;
  touch_id: TouchIdStatus;
  phone: PhoneAccessStatus;
}

export async function getTouchIdStatus(): Promise<TouchIdStatus> {
  return invoke<TouchIdStatus>("touch_id_status");
}

export async function getDesktopStatusSnapshot(): Promise<DesktopStatusSnapshot> {
  return invoke<DesktopStatusSnapshot>("desktop_status_snapshot");
}

export async function enrollTouchId(): Promise<DesktopStatusSnapshot> {
  return invoke<DesktopStatusSnapshot>("touch_id_enroll");
}

export async function armWithTouchId(): Promise<DesktopStatusSnapshot> {
  return invoke<DesktopStatusSnapshot>("touch_id_arm");
}

export async function renewTouchIdArm(): Promise<DesktopStatusSnapshot> {
  return invoke<DesktopStatusSnapshot>("touch_id_renew");
}

export async function disarmTouchId(): Promise<DesktopStatusSnapshot> {
  return invoke<DesktopStatusSnapshot>("touch_id_disarm");
}

/** Non-secret private phone publication state owned by the installed app. */
export type PhoneAccessState =
  | "off"
  | "starting"
  | "ready"
  | "published_but_backend_down"
  | "tailscale_unavailable"
  | "not_logged_in"
  | "foreign_unowned"
  | "funnel_present"
  | "interrupted_change"
  | "stopping"
  | "command_error";

export interface PhoneAccessStatus {
  state: PhoneAccessState;
  message: string;
  tailnet_url: string | null;
  enabled: boolean;
  ownership: string;
  interrupted: boolean;
  gateway_routes: boolean;
  funnel_present: boolean;
}

export async function getPhoneAccessStatus(): Promise<PhoneAccessStatus> {
  return invoke<PhoneAccessStatus>("phone_access_status");
}

export async function enablePhoneAccess(): Promise<DesktopStatusSnapshot> {
  return invoke<DesktopStatusSnapshot>("phone_access_enable");
}

export async function disablePhoneAccess(): Promise<DesktopStatusSnapshot> {
  return invoke<DesktopStatusSnapshot>("phone_access_disable");
}

export async function getServerLogs(): Promise<string[]> {
  return invoke<string[]>("get_server_logs");
}

export async function clearServerLogs(): Promise<void> {
  await invoke("clear_server_logs");
}

export async function saveSynthesisNative(text: string): Promise<string> {
  return invoke<string>("save_synthesis", { text });
}

export async function savePdf(data: Uint8Array, filename: string): Promise<string> {
  return invoke<string>("save_pdf", { data, filename });
}

export async function pickFile(): Promise<string | null> {
  const picked = await invoke<string | null>("pick_file");
  return picked ?? null;
}

export async function pingCouncil(): Promise<string> {
  return invoke<string>("ping_council");
}
