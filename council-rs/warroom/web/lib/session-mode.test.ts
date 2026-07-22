import { describe, expect, it } from "vitest";
import { sessionModeBadge } from "./session-mode";
import type { SessionMode } from "./types";

describe("sessionModeBadge", () => {
  it("renders no chip for the default normal mode", () => {
    expect(sessionModeBadge("normal")).toBeNull();
  });

  it("renders no chip for an empty mode string", () => {
    expect(sessionModeBadge("")).toBeNull();
  });

  // One case per remaining SessionMode union member — typed so a union
  // change in lib/types.ts forces this table to be revisited.
  const cases: [Exclude<SessionMode, "normal">, string, string][] = [
    ["teardown", "TEARDOWN", "chip chip-teardown"],
    ["pathfind", "PATHFIND", "chip chip-success"],
    ["harden", "HARDEN", "chip chip-warning"],
    ["blind", "BLIND", "chip chip-cyan"],
    ["recall", "RECALL", "chip chip-amber"],
    ["wargame", "WARGAME", "chip chip-danger"],
    ["premortem", "PREMORTEM", "chip chip-magenta"],
    ["contrarian", "CONTRARIAN", "chip chip-danger"],
    ["munger", "MUNGER", "chip chip-amber"],
    ["kiss", "KISS", "chip chip-cyan"],
    ["specops", "SPECOPS", "chip chip-magenta"],
    ["unknown", "UNKNOWN", "chip"],
  ];

  it.each(cases)("maps %s to a labeled chip", (mode, label, chipClass) => {
    expect(sessionModeBadge(mode)).toEqual({ label, chipClass });
  });

  it("covers every non-normal SessionMode union member", () => {
    expect(cases.map(([mode]) => mode).sort()).toEqual(
      [
        "blind",
        "contrarian",
        "harden",
        "kiss",
        "munger",
        "pathfind",
        "premortem",
        "recall",
        "specops",
        "teardown",
        "unknown",
        "wargame",
      ],
    );
  });

  it("falls back to an uppercase plain chip for unrecognized modes", () => {
    expect(sessionModeBadge("bogus")).toEqual({ label: "BOGUS", chipClass: "chip" });
  });
});
