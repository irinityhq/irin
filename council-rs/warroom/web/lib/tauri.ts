/**
 * Tauri desktop bridge — all imports are dynamic so `next build` works in browser-only mode.
 */

import {
  getAuthToken,
  councilPortFromApiBase,
  getCouncilPath,
  getCouncilRoot,
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
  councilPath?: string,
  serverPort?: number,
  authToken?: string,
  councilRoot?: string,
  librarianBase?: string,
): Promise<string> {
  const path = councilPath ?? (getCouncilPath() || undefined);
  const token = authToken ?? getAuthToken();
  // feature contract: councilRoot becomes the sidecar's `--base-dir` (cabinets/prompts/
  // models.yaml root). It does NOT relocate the council binary — that stays
  // pinned to the repo's target/release (COUNCIL_RS_DIR env is the knob for a
  // fully different checkout). Tauri 2 maps camelCase → snake_case args;
  // older shells without the council_root command arg simply ignore it.
  const root = (councilRoot ?? getCouncilRoot()).trim();
  const libBase = (librarianBase ?? getLibrarianBase()).trim();
  const resolvedPort =
    serverPort ?? councilPortFromApiBase(getRuntimeConfig().apiBase);
  return invoke<string>("start_council_server", {
    councilPath: path || null,
    serverPort: resolvedPort,
    authToken: token.trim() ? token.trim() : null,
    councilRoot: root || null,
    librarianBase: libBase || null,
  });
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
  councilRoot?: string,
  librarianBase?: string,
): Promise<string> {
  return invoke<string>("restart_sidecar", {
    viaGateway,
    councilRoot: councilRoot || null,
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
  support_matrix_summary: string;
}

export function gatewayPackAllowsGoverned(
  status: GatewayPackStatus | null | undefined,
): boolean {
  return status?.state === "authenticated_ready" && status.authenticated === true;
}

export async function getGatewayPackStatus(): Promise<GatewayPackStatus> {
  return invoke<GatewayPackStatus>("gateway_pack_status");
}

export async function enableGatewayPack(): Promise<GatewayPackStatus> {
  return invoke<GatewayPackStatus>("gateway_pack_enable");
}

export async function disableGatewayPack(): Promise<GatewayPackStatus> {
  return invoke<GatewayPackStatus>("gateway_pack_disable");
}

export async function stopGatewayPack(): Promise<GatewayPackStatus> {
  return invoke<GatewayPackStatus>("gateway_pack_stop");
}

export async function uninstallGatewayPack(): Promise<GatewayPackStatus> {
  return invoke<GatewayPackStatus>("gateway_pack_uninstall");
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

/** True when running inside Tauri and council path is unset. */
export function needsCouncilPathFromSettings(): boolean {
  return isTauri() && !getRuntimeConfig().councilPath.trim();
}
