import { describe, expect, it } from "vitest";
import { ABORT_NOTICE, applyEvent, reduceDeliberationState } from "./useDeliberation";
import { initialState } from "@/lib/types";
import type { PrecedentMatch, StreamEvent } from "@/lib/types";

// Mirrors precedent::entries_to_match_values in src/precedent/mod.rs —
// the exact 9 keys Rust emits per match (feature contract).
const match: PrecedentMatch = {
  id: "council_20260101_120000",
  ts: "2026-01-01T12:00:00Z",
  topic: "Prior ruling on shipping",
  keywords: ["shipping", "risk"],
  ruling_digest: "Ship behind a flag.",
  confidence: "HIGH",
  cabinet: "warroom",
  convergence: 0.91,
  mode: "teardown",
};

function precedentLoadedEvent(matches: PrecedentMatch[]): StreamEvent {
  return {
    type: "precedent_loaded",
    session_id: "council_20260606_090000",
    ts: "2026-06-06T09:00:00Z",
    data: { matches },
  };
}

describe("applyEvent precedent_loaded", () => {
  it("populates state.precedent from the matches array", () => {
    const streaming = { ...initialState, phase: "streaming" as const };
    const next = applyEvent(streaming, precedentLoadedEvent([match]));
    expect(next.precedent).toEqual([match]);
  });

  it("leaves the rest of the state untouched", () => {
    const streaming = {
      ...initialState,
      phase: "streaming" as const,
      session_id: "council_20260606_090000",
      topic: "Should we ship the thing?",
    };
    const next = applyEvent(streaming, precedentLoadedEvent([match]));
    expect(next.phase).toBe("streaming");
    expect(next.session_id).toBe("council_20260606_090000");
    expect(next.topic).toBe("Should we ship the thing?");
    expect(next.rounds).toEqual([]);
  });

  it("does not mutate the previous state", () => {
    const streaming = { ...initialState, phase: "streaming" as const };
    applyEvent(streaming, precedentLoadedEvent([match]));
    expect(streaming.precedent).toEqual([]);
  });

  it("replaces precedent wholesale on a second event", () => {
    const streaming = { ...initialState, phase: "streaming" as const };
    const withOne = applyEvent(streaming, precedentLoadedEvent([match]));
    const second: PrecedentMatch = { ...match, id: "council_20260102_080000" };
    const next = applyEvent(withOne, precedentLoadedEvent([second]));
    expect(next.precedent).toEqual([second]);
  });
});

describe("session route provenance", () => {
  it("records the backend-enforced governed route and sensitivity", () => {
    const next = applyEvent(initialState, {
      type: "session_started",
      session_id: "governed-session",
      ts: "2026-07-16T12:00:00Z",
      data: {
        topic: "Govern this proceeding",
        cabinet_name: "standard",
        rounds_planned: 1,
        mode: "normal",
        active_seats: [],
        dropped_seats: [],
        chair: { provider: "grok", model: "grok-4.3" },
        available_providers: ["grok"],
        council_version: "10",
        stream_version: "rs-1.0.0",
        via_gateway: true,
        execution_route: "governed",
        sensitivity: "yellow",
      },
    });

    expect(next.execution_route).toBe("governed");
    expect(next.gateway_sensitivity).toBe("yellow");
  });

  it("falls back to the legacy via_gateway flag for older servers", () => {
    const next = applyEvent(initialState, {
      type: "session_started",
      session_id: "legacy-direct-session",
      ts: "2026-07-16T12:00:00Z",
      data: {
        topic: "Direct proceeding",
        cabinet_name: "standard",
        rounds_planned: 1,
        mode: "normal",
        active_seats: [],
        dropped_seats: [],
        chair: { provider: "grok", model: "grok-4.3" },
        available_providers: ["grok"],
        council_version: "10",
        stream_version: "rs-1.0.0",
        via_gateway: false,
      },
    });

    expect(next.execution_route).toBe("direct");
  });
});

describe("seat Gateway provenance", () => {
  it("retains the Rust SeatResponse.gateway wire field", () => {
    const started = applyEvent(initialState, {
      type: "session_started",
      session_id: "governed-session",
      ts: "2026-07-16T12:00:00Z",
      data: {
        topic: "Govern this proceeding",
        cabinet_name: "standard",
        rounds_planned: 1,
        mode: "normal",
        active_seats: [{ name: "Analyst", provider: "grok", model: "grok-4.3" }],
        dropped_seats: [],
        chair: { provider: "claude", model: "claude-opus-4-8" },
        available_providers: ["grok", "claude"],
        council_version: "10",
        stream_version: "rs-1.0.0",
        execution_route: "governed",
      },
    });
    const round = applyEvent(started, {
      type: "round_started",
      session_id: "governed-session",
      ts: "2026-07-16T12:00:01Z",
      data: { round_num: 1 },
    });
    const complete = applyEvent(round, {
      type: "seat_complete",
      session_id: "governed-session",
      ts: "2026-07-16T12:00:02Z",
      data: {
        seat_name: "Analyst",
        provider: "grok",
        model: "grok-4.3",
        text: "Analysis",
        round_num: 1,
        latency_ms: 10,
        tokens_in: 5,
        tokens_out: 5,
        cached_in: 0,
        cost_usd: 0.01,
        gateway: {
          gateway_request_id: "gw-seat-123",
          routed_model: "grok-4.3",
          routed_provider: "xai",
          fallback_used: false,
        },
      },
    });

    expect(complete.rounds[0].seats.Analyst.gateway_provenance?.gateway_request_id).toBe(
      "gw-seat-123",
    );
  });
});

describe("completed deliberation terminal state", () => {
  it("ignores a late transport error after done", () => {
    const done = {
      ...initialState,
      phase: "done" as const,
      synthesis: {
        text: "Final ruling",
        model: "chair",
        latency_ms: 10,
        cost_usd: 0,
      },
    };

    const next = reduceDeliberationState(done, {
      kind: "fatal",
      message: "WebSocket error",
    });

    expect(next).toBe(done);
  });

  it("ignores a late fatal stream error after done", () => {
    const done = {
      ...initialState,
      phase: "done" as const,
      synthesis: {
        text: "Final ruling",
        model: "chair",
        latency_ms: 10,
        cost_usd: 0,
      },
    };

    const next = applyEvent(done, {
      type: "error",
      session_id: "council_20260606_090000",
      ts: "2026-06-06T09:00:00Z",
      data: { message: "late close", fatal: true },
    });

    expect(next).toBe(done);
  });
});

describe("operator Abort", () => {
  it("halts the active UI with an honest provider-charge warning", () => {
    const streaming = {
      ...initialState,
      phase: "streaming" as const,
      session_id: "active-session",
      topic: "Stop this",
    };

    const next = reduceDeliberationState(streaming, { kind: "aborted" });

    expect(next.phase).toBe("error");
    expect(next.errors.at(-1)).toMatchObject({
      message: ABORT_NOTICE,
      fatal: true,
    });
    expect(next.session_id).toBe("active-session");
  });

  it("does not replace a completed run with an Abort state", () => {
    const done = { ...initialState, phase: "done" as const };
    expect(reduceDeliberationState(done, { kind: "aborted" })).toBe(done);
  });
});
