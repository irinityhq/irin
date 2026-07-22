import { describe, expect, it } from "vitest";
import { applyEvent } from "./useDeliberation";
import { initialState } from "@/lib/types";
import type {
  DeliberationState, SeatChunkData, SeatCompleteData, SeatRef, StreamEvent,
} from "@/lib/types";

const SEATS: SeatRef[] = [
  { name: "Strategist", provider: "grok", model: "grok-4.3" },
  { name: "Critic", provider: "claude", model: "claude-x" },
];

/** Build a streaming state with one started round and pending seats. */
function startedRound(): DeliberationState {
  let s: DeliberationState = { ...initialState };
  s = applyEvent(s, {
    type: "session_started",
    session_id: "council_x",
    ts: "t",
    data: {
      topic: "T",
      cabinet_name: "warroom",
      rounds_planned: 2,
      mode: "normal",
      active_seats: SEATS,
      dropped_seats: [],
      chair: { provider: "claude", model: "c" },
      available_providers: [],
      council_version: "",
      stream_version: "",
    },
  });
  s = applyEvent(s, {
    type: "round_started",
    session_id: "council_x",
    ts: "t",
    data: { round_num: 1 },
  });
  return s;
}

function chunk(d: SeatChunkData): StreamEvent {
  return { type: "seat_chunk", session_id: "council_x", ts: "t", data: d };
}

function complete(d: SeatCompleteData): StreamEvent {
  return { type: "seat_complete", session_id: "council_x", ts: "t", data: d };
}

const baseComplete: SeatCompleteData = {
  seat_name: "Strategist",
  provider: "grok",
  model: "grok-4.3",
  text: "FINAL authoritative text",
  round_num: 1,
  latency_ms: 100,
  tokens_in: 10,
  tokens_out: 20,
  cached_in: 0,
  cost_usd: 0.001,
  error: null,
};

describe("applyEvent seat_chunk", () => {
  it("accumulates deltas per seat in arrival order, marking it streaming", () => {
    let s = startedRound();
    s = applyEvent(s, chunk({ seat_name: "Strategist", round_num: 1, text_delta: "Hel", seq: 0 }));
    s = applyEvent(s, chunk({ seat_name: "Strategist", round_num: 1, text_delta: "lo ", seq: 1 }));
    s = applyEvent(s, chunk({ seat_name: "Strategist", round_num: 1, text_delta: "world", seq: 2 }));
    const seat = s.rounds[0].seats["Strategist"];
    expect(seat.text).toBe("Hello world");
    expect(seat.streaming).toBe(true);
    expect(seat.status).toBe("thinking");
  });

  it("does not bleed chunks across seats", () => {
    let s = startedRound();
    s = applyEvent(s, chunk({ seat_name: "Strategist", round_num: 1, text_delta: "A", seq: 0 }));
    s = applyEvent(s, chunk({ seat_name: "Critic", round_num: 1, text_delta: "B", seq: 0 }));
    expect(s.rounds[0].seats["Strategist"].text).toBe("A");
    expect(s.rounds[0].seats["Critic"].text).toBe("B");
  });

  it("seat_complete replaces accumulated chunks with authoritative text", () => {
    let s = startedRound();
    s = applyEvent(s, chunk({ seat_name: "Strategist", round_num: 1, text_delta: "partial draft", seq: 0 }));
    s = applyEvent(s, complete(baseComplete));
    const seat = s.rounds[0].seats["Strategist"];
    expect(seat.text).toBe("FINAL authoritative text");
    expect(seat.status).toBe("complete");
    expect(seat.streaming).toBe(false);
  });

  it("complete with zero chunks renders the authoritative text (non-streaming provider)", () => {
    let s = startedRound();
    // No chunks at all — non-streaming provider path.
    s = applyEvent(s, complete(baseComplete));
    const seat = s.rounds[0].seats["Strategist"];
    expect(seat.text).toBe("FINAL authoritative text");
    expect(seat.status).toBe("complete");
    expect(seat.streaming).toBe(false);
  });

  it("drops duplicate/stale seq (out-of-order tolerance via seq guard)", () => {
    let s = startedRound();
    s = applyEvent(s, chunk({ seat_name: "Strategist", round_num: 1, text_delta: "one", seq: 0 }));
    s = applyEvent(s, chunk({ seat_name: "Strategist", round_num: 1, text_delta: "two", seq: 1 }));
    // Replay of seq 0 and 1 must be ignored.
    s = applyEvent(s, chunk({ seat_name: "Strategist", round_num: 1, text_delta: "dup", seq: 1 }));
    s = applyEvent(s, chunk({ seat_name: "Strategist", round_num: 1, text_delta: "older", seq: 0 }));
    expect(s.rounds[0].seats["Strategist"].text).toBe("onetwo");
  });

  it("ignores chunks for an unknown seat without throwing", () => {
    let s = startedRound();
    const before = s;
    s = applyEvent(s, chunk({ seat_name: "Ghost", round_num: 1, text_delta: "x", seq: 0 }));
    expect(s.rounds[0].seats).toEqual(before.rounds[0].seats);
  });

  it("does not mutate the previous state", () => {
    const s = startedRound();
    applyEvent(s, chunk({ seat_name: "Strategist", round_num: 1, text_delta: "x", seq: 0 }));
    expect(s.rounds[0].seats["Strategist"].text).toBe("");
  });
});

describe("applyEvent round_divergence", () => {
  function divergence(round_num: number, points: { seat: string; x: number; y: number }[]): StreamEvent {
    return {
      type: "round_divergence",
      session_id: "council_x",
      ts: "t",
      data: { round_num, method: "pca", points },
    };
  }

  it("stores the projected points on the matching round", () => {
    let s = startedRound();
    s = applyEvent(s, divergence(1, [
      { seat: "Strategist", x: 0.1, y: -0.2 },
      { seat: "Critic", x: -0.3, y: 0.4 },
    ]));
    expect(s.rounds[0].divergence).toEqual([
      { seat: "Strategist", x: 0.1, y: -0.2 },
      { seat: "Critic", x: -0.3, y: 0.4 },
    ]);
  });

  it("tolerates an empty points array", () => {
    let s = startedRound();
    s = applyEvent(s, divergence(1, []));
    expect(s.rounds[0].divergence).toEqual([]);
  });

  it("is a no-op for an unknown round", () => {
    let s = startedRound();
    const before = s.rounds[0].divergence;
    s = applyEvent(s, divergence(99, [{ seat: "Strategist", x: 0, y: 0 }]));
    expect(s.rounds[0].divergence).toBe(before);
  });
});
