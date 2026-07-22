import type { DirectFireMode, GatewaySensitivity, StartPayload } from "./ws";
import { gatewayStartFields } from "./gateway-mode";

/** UI metadata for one direct-fire mode (feature contract). */
export interface DirectFireModeInfo {
  mode: DirectFireMode;
  label: string;
  /** CLI flag this mode mirrors — shown as a parity hint. */
  cliFlag: string;
  description: string;
  /** Experimental modes render the kill-criteria banner. */
  experimental?: boolean;
}

/**
 * All direct-fire modes, in CLI documentation order. Pinned to the
 * `DirectFireMode` union — adding a wire value forces an entry here.
 */
export const DIRECT_FIRE_MODES: readonly DirectFireModeInfo[] = [
  {
    mode: "contrarian",
    label: "Contrarian",
    cliFlag: "--contrarian",
    description: "Tear this down",
  },
  {
    mode: "munger",
    label: "Munger",
    cliFlag: "--munger",
    description: "Invert the thesis",
  },
  {
    mode: "kiss",
    label: "KISS",
    cliFlag: "--kiss-review",
    description: "Simplicity review",
  },
  {
    mode: "specops",
    label: "SpecOps",
    cliFlag: "--specops",
    description: "Signal from noise",
  },
  {
    mode: "premortem",
    label: "Premortem",
    cliFlag: "--premortem",
    description: "Assume failure, work backwards",
    experimental: true,
  },
];

/**
 * Build the WS start payload for a single-shot direct fire.
 *
 * Contract notes (feature contract):
 * - `direct_fire` is the pinned wire field; the server runs ONE model and
 *   emits `session_started → synthesis_started → synthesis_complete →
 *   session_saved → done` with no round events.
 * - `cabinet_name` must still be a valid registry key (the parser and smoke
 *   shim load it) even though the single-shot path convenes no seats —
 *   "standard" is the safe default.
 * - The route is always explicit via `gatewayStartFields`; Direct must not
 *   inherit an invisible server process default.
 */
export function buildDirectFireStartPayload(opts: {
  topic: string;
  mode: DirectFireMode;
  context?: string;
  viaGateway?: boolean;
  sensitivity?: GatewaySensitivity;
}): StartPayload {
  return {
    topic: opts.topic.trim(),
    cabinet_name: "standard",
    direct_fire: opts.mode,
    context: opts.context?.trim() || undefined,
    ...gatewayStartFields(opts.viaGateway ?? false, opts.sensitivity),
  };
}
