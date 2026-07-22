import { describe, expect, it } from "vitest";
import {
  applyLibrarianEvent,
  emptyPendingTurn,
  type LibrarianPendingTurn,
  type LibrarianWsEvent,
} from "./librarian-ws";
import type { LibrarianMessage, LibrarianSource } from "./librarian";

const SOURCES: LibrarianSource[] = [
  { path: "notes/a.md", score: 0.91, snippet: "…" },
];

const FINAL: LibrarianMessage = {
  type: "assistant",
  id: "m1",
  content: "Full buffered answer",
  ts: "t",
  sources: SOURCES,
  model: "research-x",
};

function run(events: LibrarianWsEvent[]): LibrarianPendingTurn {
  return events.reduce(applyLibrarianEvent, emptyPendingTurn());
}

describe("applyLibrarianEvent", () => {
  it("accumulates ask_chunk deltas while streaming", () => {
    const turn = run([
      { type: "ask_started" },
      { type: "ask_chunk", text_delta: "Hel" },
      { type: "ask_chunk", text_delta: "lo" },
    ]);
    expect(turn.text).toBe("Hello");
    expect(turn.streaming).toBe(true);
  });

  it("populates sources on the sources frame", () => {
    const turn = run([
      { type: "ask_started" },
      { type: "ask_chunk", text_delta: "x" },
      { type: "sources", sources: SOURCES },
    ]);
    expect(turn.sources).toEqual(SOURCES);
  });

  it("ask_complete stops streaming (final message is authoritative)", () => {
    const turn = run([
      { type: "ask_started" },
      { type: "ask_chunk", text_delta: "preview" },
      { type: "ask_complete", message: FINAL },
    ]);
    expect(turn.streaming).toBe(false);
  });

  it("zero-chunk flow: ask_started -> ask_complete leaves text empty and not streaming", () => {
    // Honest compliant path — librarian upstream is a single buffered POST.
    const turn = run([
      { type: "ask_started" },
      { type: "ask_complete", message: FINAL },
      { type: "done" },
    ]);
    expect(turn.text).toBe("");
    expect(turn.streaming).toBe(false);
  });

  it("error frame stops streaming", () => {
    const turn = run([
      { type: "ask_started" },
      { type: "error", message: "upstream down" },
    ]);
    expect(turn.streaming).toBe(false);
  });

  it("ask_started resets a previous turn's accumulation", () => {
    const dirty: LibrarianPendingTurn = { text: "stale", sources: SOURCES, streaming: false };
    const turn = applyLibrarianEvent(dirty, { type: "ask_started" });
    expect(turn).toEqual({ text: "", sources: [], streaming: true });
  });

  it("does not mutate the previous turn", () => {
    const start = emptyPendingTurn();
    applyLibrarianEvent(start, { type: "ask_chunk", text_delta: "x" });
    expect(start.text).toBe("");
  });
});
