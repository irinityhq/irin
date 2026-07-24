/**
 * Client helpers for the installed-release Gateway Pack.
 * Status is non-secret; raw GW_API_KEY never crosses this boundary.
 */

import type { GatewayPackState, GatewayPackStatus } from "./tauri";
import { gatewayPackAllowsGoverned } from "./tauri";

export type { GatewayPackState, GatewayPackStatus };
export { gatewayPackAllowsGoverned };

/** Operator-facing label for a pack state (never "ready" for a bare URL). */
export function gatewayPackStateLabel(state: GatewayPackState): string {
  switch (state) {
    case "not_installed":
      return "Not installed";
    case "docker_missing":
      return "Docker missing";
    case "docker_daemon_down":
      return "Docker daemon down";
    case "installing":
      return "Installing";
    case "installed_stopped":
      return "Installed · stopped";
    case "starting":
      return "Starting";
    case "authenticated_ready":
      return "Authenticated ready";
    case "degraded":
      return "Degraded";
    case "disabled":
      return "Disabled";
    default:
      return state;
  }
}

/** Core War Room must stay non-red for these optional-path states. */
export function gatewayPackIsCoreNeutral(state: GatewayPackState): boolean {
  return (
    state === "not_installed" ||
    state === "docker_missing" ||
    state === "docker_daemon_down" ||
    state === "disabled" ||
    state === "installed_stopped"
  );
}

/** Whether the Deliberate "Governed via Gateway" toggle may turn on. */
export function canEnableGovernedProceeding(
  status: GatewayPackStatus | null | undefined,
  opts?: { requireInstalledRelease?: boolean; desktopMode?: string },
): boolean {
  if (opts?.requireInstalledRelease && opts.desktopMode === "installed-release") {
    return gatewayPackAllowsGoverned(status);
  }
  // Browser / development: do not hard-block (debug may use external Gateway).
  if (opts?.desktopMode === "development") {
    return true;
  }
  // Tauri build mode still detecting or unavailable: fail closed — governed
  // proceeding must not present as available before the mode is known.
  if (opts?.desktopMode === "detecting" || opts?.desktopMode === "unavailable") {
    return false;
  }
  if (!status) {
    // Unknown pack status outside Tauri: leave toggle free (Direct is default).
    return true;
  }
  return gatewayPackAllowsGoverned(status);
}

/**
 * Header/status strip label: distinguish URL configured, pack authenticated,
 * and Council actually governed. Never call a bare URL "ready".
 */
export type GatewayHeaderTruth =
  | { label: string; tone: "ok" | "warn" | "down" | "neutral"; detail: string };

export function gatewayHeaderTruth(
  pack: GatewayPackStatus | null | undefined,
  healthGatewayConfigured: boolean,
): GatewayHeaderTruth {
  if (pack?.state === "authenticated_ready" && pack.authenticated && pack.council_governed) {
    return {
      label: "governed",
      tone: "ok",
      detail: "Gateway Pack authenticated and Council is governed.",
    };
  }
  if (pack?.authenticated && pack.enabled) {
    return {
      label: "pack auth",
      tone: "warn",
      detail: "Gateway client authenticates; Council governed route not confirmed.",
    };
  }
  if (pack?.state === "docker_missing" || pack?.state === "docker_daemon_down") {
    return {
      label: "direct",
      tone: "neutral",
      detail: pack.message || "Docker optional; core War Room is Direct.",
    };
  }
  if (healthGatewayConfigured) {
    return {
      label: "url set",
      tone: "warn",
      detail:
        "Gateway credentials present on Council health (not the same as Pack authenticated-ready).",
    };
  }
  return {
    label: "not set",
    tone: "down",
    detail: "No Gateway client key; Direct mode.",
  };
}
