import type { ForkResult, SeatSwap } from "./types";
import type { StartPayload } from "./ws";

const OVERRIDE_FIELDS = ["provider", "model", "system"] as const;

/**
 * Build the WebSocket start payload for a forked deliberation from a resolved
 * `POST /api/sessions/{id}/fork` response.
 *
 * Contract notes (feature contract):
 * - `cabinet_name` must be the parent's REGISTRY KEY (`parent_cabinet_key`),
 *   never the display label (`parent_cabinet_label`).
 * - `custom_cabinet.name` must be the display + provenance label. Rust's
 *   `Cabinet` has no `label` field (the fork response's `cabinet.label` is
 *   silently dropped on deserialization) and the backend persists
 *   `cabinet_name = custom_cabinet.name` into the session record, so leaving
 *   the registry key in `name` would mix keys ("warroom") and display labels
 *   ("War Room") in the append-only catalog + precedent index and lose the
 *   "(fork of …)" provenance.
 * - Swaps are applied client-side to a deep copy of the cabinet (so the user
 *   sees the exact config being launched) AND passed through as `swaps` so
 *   the backend records lineage with what changed.
 * - Edits that carry no override field (provider/model/system) are dropped.
 */
export function buildForkStartPayload(
  resolved: ForkResult,
  edits: Record<string, SeatSwap>,
  pauseAfterEachRound: boolean,
): StartPayload {
  const swapList: SeatSwap[] = Object.values(edits).filter((e) =>
    OVERRIDE_FIELDS.some((field) => e[field] !== undefined),
  );
  const cabinet = JSON.parse(JSON.stringify(resolved.cabinet)) as ForkResult["cabinet"];
  // Restore the display + fork provenance label (see contract notes above):
  // this string becomes the forked session's persisted cabinet_name.
  cabinet.name = `${resolved.parent_cabinet_label} (fork of ${resolved.parent_id})`;
  for (const sw of swapList) {
    const i = cabinet.seats.findIndex((s) => s.name === sw.seat_name);
    if (i < 0) continue;
    OVERRIDE_FIELDS.forEach((field) => {
      const value = sw[field];
      if (value !== undefined) cabinet.seats[i][field] = value;
    });
  }
  return {
    topic: resolved.topic,
    cabinet_name: resolved.parent_cabinet_key,
    custom_cabinet: cabinet,
    parent_session_id: resolved.parent_id,
    swaps: swapList,
    pause_after_each_round: pauseAfterEachRound,
  };
}
