import { describe, expect, it } from "vitest";
import { applyEvent } from "./useDeliberation";
import { initialState } from "@/lib/types";
import type { DeliberationState, StreamEvent } from "@/lib/types";

// Pinned feature contract single-shot wire sequence — no round/seat events.
const SESSION_ID = "council_20260606_120000";

function ev(type: StreamEvent["type"], data: unknown): StreamEvent {
  return { type, session_id: SESSION_ID, ts: "2026-06-06T12:00:00Z", data };
}

const singleShot: StreamEvent[] = [
  ev("session_started", {
    topic: "Tear down this plan",
    cabinet_name: "standard",
    rounds_planned: 0,
    mode: "normal",
    active_seats: [],
    dropped_seats: [],
    chair: { provider: "grok", model: "grok-4.3" },
    available_providers: ["grok"],
    council_version: "10",
    stream_version: "rs-1.0.0",
  }),
  ev("synthesis_started", { model: "grok-4.3" }),
  ev("synthesis_complete", {
    text: "## Verdict\nKill it.",
    model: "grok-4.3",
    latency_ms: 1200,
    cost_usd: 0.01,
  }),
  ev("session_saved", { path: "sessions/council_20260606_120000.json" }),
  ev("done", {
    total_tokens: 500,
    total_cost_usd: 0.01,
    total_latency_ms: 1200,
    synthesis: "## Verdict\nKill it.",
    session_id: SESSION_ID,
    convergence_final: 0,
    rounds_run: 0,
  }),
];

function run(events: StreamEvent[]): DeliberationState {
  return events.reduce(applyEvent, initialState);
}

describe("applyEvent direct-fire single-shot sequence", () => {
  it("lands in done with the synthesis populated", () => {
    const s = run(singleShot);
    expect(s.phase).toBe("done");
    expect(s.synthesis?.text).toBe("## Verdict\nKill it.");
    expect(s.synthesis?.model).toBe("grok-4.3");
  });

  it("never creates rounds (no round UI to render)", () => {
    const s = run(singleShot);
    expect(s.rounds).toEqual([]);
    expect(s.current_round).toBe(0);
  });

  it("records the saved session path", () => {
    const s = run(singleShot);
    expect(s.saved_path).toBe("sessions/council_20260606_120000.json");
  });

  it("takes totals from the done event", () => {
    const s = run(singleShot);
    expect(s.totals).toEqual({ tokens: 500, cost_usd: 0.01, latency_ms: 1200 });
  });

  it("passes through synthesizing during the shot", () => {
    const synthesizing = run(singleShot.slice(0, 2));
    expect(synthesizing.phase).toBe("synthesizing");
  });
});
