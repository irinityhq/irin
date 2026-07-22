import { getAuthToken, getWsBase } from "./runtime-config";
import type { InterventionPayload, SeatSwap, StreamEvent } from "./types";

/**
 * Gateway sensitivity wire values (feature contract). Lowercase ON THE WIRE — the
 * backend normalizes to the uppercase `X-Sensitivity-Level` gateway header.
 */
export type GatewaySensitivity = "green" | "yellow" | "red";

/**
 * Direct-fire single-shot modes (feature contract) — CLI parity for `--contrarian`,
 * `--munger`, `--kiss-review`, `--specops`, `--premortem`. When set on the
 * start payload the server runs ONE model, no council rounds, and emits
 * `session_started → synthesis_started → synthesis_complete → session_saved
 * → done`.
 */
export type DirectFireMode =
  | "contrarian"
  | "munger"
  | "kiss"
  | "specops"
  | "premortem";

export interface StartPayload {
  topic: string;
  cabinet_name: string;
  custom_cabinet?: unknown;
  context?: string;
  map_dir?: string;
  blind?: boolean;
  max_rounds?: number;
  pause_after_each_round?: boolean;
  parent_session_id?: string;
  swaps?: SeatSwap[];
  mode?: "teardown" | "pathfind" | "harden";
  validate?: boolean;
  validate_provider?: string;
  validate_gate?: boolean;
  frame_check?: boolean;
  /** Max USD spend before stream pauses (mirrors CLI `--budget`). */
  budget_max_usd?: number;
  /** Provider routing tier: best | sovereign | strict_sovereign */
  tier?: string;
  /** After Pathfind completes, auto-run TearDown (CLI `--then-tear-down`). */
  then_tear_down?: boolean;
  /**
   * Per-session gateway routing (feature contract). OMIT to fall back to the server's
   * `COUNCIL_VIA_GATEWAY` process env — sending `false` would override an
   * env-enabled gateway.
   */
  via_gateway?: boolean;
  /** Gateway sensitivity when via_gateway is enabled (lowercase wire value). */
  sensitivity?: GatewaySensitivity;
  /** Single-shot direct fire — no council rounds (feature contract). */
  direct_fire?: DirectFireMode;
  /** Auto SpecOps when final convergence is below this threshold (default 0.8). */
  auto_specops_threshold?: number;
  /** Optional triage worker provenance guard (JSON). */
  worker_provenance?: Record<string, unknown>;
  /** E2E smoke only — honored when backend has COUNCIL_WS_SMOKE_ONLY=1 */
  smoke_only?: boolean;
}

export interface DeliberationSocket {
  send_intervention: (p: InterventionPayload) => void;
  close: () => void;
  ws: WebSocket;
}

export function openDeliberation(
  start: StartPayload,
  onEvent: (ev: StreamEvent) => void,
  onError: (msg: string) => void,
  onClose: () => void,
): DeliberationSocket {
  const url = `${getWsBase()}/ws/deliberate`;
  const token = getAuthToken();
  const protocols = token ? ["council", `token.${token}`] : undefined;
  const ws = protocols ? new WebSocket(url, protocols) : new WebSocket(url);

  ws.addEventListener("open", () => {
    ws.send(JSON.stringify({ type: "start", payload: start }));
  });

  ws.addEventListener("message", (msg) => {
    try {
      const ev = JSON.parse(msg.data) as StreamEvent;
      onEvent(ev);
    } catch (e) {
      onError(`Malformed event: ${(e as Error).message}`);
    }
  });

  ws.addEventListener("error", () => onError("WebSocket error"));
  ws.addEventListener("close", () => onClose());

  return {
    ws,
    send_intervention: (p: InterventionPayload) => {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: "intervention", payload: p }));
      }
    },
    close: () => {
      try { ws.close(); } catch { /* noop */ }
    },
  };
}