import type { GatewaySensitivity, StartPayload } from "./ws";

/**
 * All gateway sensitivity wire values, in escalation order. Lowercase on the
 * wire (pinned feature contract contract); the UI displays them uppercase to match the
 * CLI's `--sensitivity GREEN|YELLOW|RED` and the `X-Sensitivity-Level`
 * header the backend derives from the wire value.
 */
export const SENSITIVITY_LEVELS: readonly GatewaySensitivity[] = [
  "green",
  "yellow",
  "red",
];

export const DEFAULT_SENSITIVITY: GatewaySensitivity = "green";

/**
 * Build the gateway fields for a WS start payload.
 *
 * Contract notes (feature contract):
 * - The War Room always sends an explicit route. OFF means direct even if the
 *   server process has a Gateway default; interactive proceedings must never
 *   inherit an invisible transport choice.
 * - When ON, `via_gateway: true` plus the lowercase sensitivity is sent;
 *   the backend uppercases it for the gateway's `X-Sensitivity-Level` header.
 */
export function gatewayStartFields(
  viaGateway: boolean,
  sensitivity: GatewaySensitivity = DEFAULT_SENSITIVITY,
): Pick<StartPayload, "via_gateway" | "sensitivity"> {
  if (!viaGateway) return { via_gateway: false };
  return { via_gateway: true, sensitivity };
}
