import { describe, expect, it } from "vitest";
import { buildForkStartPayload } from "./fork-launch";
import type { ForkResult, SeatSwap } from "./types";

// Mirrors the feature contract fork response: cabinet.name is the REGISTRY KEY,
// cabinet.label is the display + provenance label (fork.rs sets
// "{label} (fork of {id})" before serializing), parent_cabinet_key/label
// sit alongside.
function forkResult(): ForkResult {
  return {
    topic: "Should we ship the thing?",
    cabinet: {
      name: "warroom",
      label: "War Room (fork of council_20260101_120000)",
      description: "High-stakes cabinet",
      seats: [
        { name: "Hawk", provider: "grok", model: "grok-4", system: "Be hawkish." },
        { name: "Dove", provider: "claude", model: "claude-opus", system: "Be dovish." },
      ],
      chair: { provider: "claude", model: "claude-opus" },
      rounds: 3,
      is_triad: false,
    },
    parent_id: "council_20260101_120000",
    parent_cabinet_label: "War Room",
    parent_cabinet_key: "warroom",
    swaps_applied: [],
  };
}

describe("buildForkStartPayload", () => {
  it("uses the parent cabinet registry key, not the display label", () => {
    const r = forkResult();
    r.parent_cabinet_label = "War Room (pretty)";
    const payload = buildForkStartPayload(r, {}, true);
    expect(payload.cabinet_name).toBe("warroom");
    expect(payload.cabinet_name).not.toBe(r.parent_cabinet_label);
  });

  it("restores the display/provenance label as custom_cabinet.name", () => {
    // Rust Cabinet has no `label` field, and the backend persists
    // cabinet_name = custom_cabinet.name — so `name` must carry the
    // "{display label} (fork of {parent_id})" string, not the registry key.
    const payload = buildForkStartPayload(forkResult(), {}, true);
    const cabinet = payload.custom_cabinet as ForkResult["cabinet"];
    expect(cabinet.name).toBe("War Room (fork of council_20260101_120000)");
    // The registry key still rides in cabinet_name for launch routing.
    expect(payload.cabinet_name).toBe("warroom");
  });

  it("carries topic, parent_session_id and pause flag through", () => {
    const payload = buildForkStartPayload(forkResult(), {}, false);
    expect(payload.topic).toBe("Should we ship the thing?");
    expect(payload.parent_session_id).toBe("council_20260101_120000");
    expect(payload.pause_after_each_round).toBe(false);
    expect(payload.swaps).toEqual([]);
  });

  it("applies seat swaps to a deep copy of the cabinet", () => {
    const r = forkResult();
    const edits: Record<string, SeatSwap> = {
      Hawk: { seat_name: "Hawk", model: "grok-5" },
    };
    const payload = buildForkStartPayload(r, edits, true);
    const cabinet = payload.custom_cabinet as ForkResult["cabinet"];
    expect(cabinet.seats[0].model).toBe("grok-5");
    expect(cabinet.seats[0].provider).toBe("grok"); // untouched field kept
    // Original response is not mutated.
    expect(r.cabinet.seats[0].model).toBe("grok-4");
    expect(payload.swaps).toEqual([{ seat_name: "Hawk", model: "grok-5" }]);
  });

  it("drops edits that carry no override field", () => {
    const edits: Record<string, SeatSwap> = {
      Hawk: { seat_name: "Hawk" }, // user focused the seat but changed nothing
      Dove: { seat_name: "Dove", system: "Be extra dovish." },
    };
    const payload = buildForkStartPayload(forkResult(), edits, true);
    expect(payload.swaps).toEqual([{ seat_name: "Dove", system: "Be extra dovish." }]);
    const cabinet = payload.custom_cabinet as ForkResult["cabinet"];
    expect(cabinet.seats[1].system).toBe("Be extra dovish.");
  });

  it("ignores swaps naming a seat that is not in the cabinet", () => {
    const edits: Record<string, SeatSwap> = {
      Ghost: { seat_name: "Ghost", model: "phantom-1" },
    };
    const payload = buildForkStartPayload(forkResult(), edits, true);
    const cabinet = payload.custom_cabinet as ForkResult["cabinet"];
    expect(cabinet.seats.map((s) => s.model)).toEqual(["grok-4", "claude-opus"]);
    // Still recorded in swaps so the backend can log the attempt.
    expect(payload.swaps).toEqual([{ seat_name: "Ghost", model: "phantom-1" }]);
  });
});
